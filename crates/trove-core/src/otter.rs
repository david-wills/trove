//! Otter.ai — AI meeting transcription service; import path via SRT/TXT
//! exports from the Otter dashboard.
//!
//! The Connect API v2 is Enterprise-only (account-manager enablement, sales-
//! gated) — not buildable for general users. The manual-export path is what
//! ships: the user exports from the Otter dashboard and drops the file into
//! the registry-driven import box. No auth, no TCC, no networking —
//! standalone-clean by construction.
//!
//! # Accepted formats
//!
//! | Extension | Plan gating | What's parsed |
//! |---|---|---|
//! | `.srt` | paid plans | Standard SubRip; `ts` from the first timestamp; speaker labels extracted when the text line is prefixed `Speaker Name: text`; transcript sidecar written (one utterance per JSONL line) |
//! | `.txt` | all plans incl. Basic | Plain text, no timestamps → `ts` from file mtime (or import date), no transcript sidecar; **Needs-sample for layout details** |
//!
//! DOCX/PDF are accepted by Otter's paid plans but their layouts are
//! undocumented — parked behind `Needs-sample`; not parsed here.
//!
//! # Vault layout
//!
//! - **Contract:** `meetings/otter/YYYY-MM.jsonl` — one [`Meeting`] per file,
//!   upserted by `guid`. `guid` = `sha256("<filename>|<content>")[..16]` (a
//!   stable content-based id; re-importing the same file is a no-op).
//! - **Raw (verbatim copy):** `meetings/otter/raw/<guid>.<ext>` — the import
//!   file stored verbatim; `transcript_ref` on the SRT row points here.
//!
//! # Privacy
//!
//! Meeting transcripts are conversation content (≈ message bodies). The def
//! ships `default_on: false`; the hub renders the acknowledgement gate.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::meetings::Meeting;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// The source id (also the vault sub-folder name).
const SOURCE: &str = "otter";
/// Contract layer: one Meeting per imported file, month-partitioned.
const CONTRACT_DIR: &str = "meetings/otter";
/// Raw layer: verbatim import files stored for reference.
const RAW_DIR: &str = "meetings/otter/raw";

// ---------------------------------------------------------------------------
// Registry.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "otter",
        name: "Otter.ai",
        kind: IntegrationKind::Import,
        // 🔒 Opt-in: meeting transcripts are conversation content (≈ message
        // bodies). The hub renders the acknowledgement gate for default-off.
        default_on: false,
        description: "Imports Otter.ai transcript exports into the unified meetings store. \
                      SRT exports (paid plans) carry timestamps and are parsed into a full \
                      transcript sidecar; TXT exports (all plans) are stored verbatim. \
                      Re-runnable: importing the same file twice is a no-op.",
        domain: "meetings",
        vault_path: "meetings/otter/",
        toggleable: false,
        setup: &[
            "In Otter.ai, open the conversation you want to export.",
            "Click the three-dot menu → Export → SRT (recommended: keeps timestamps) \
             or TXT (Basic plan).",
            "Import the downloaded file here. Re-importing the same export is safe.",
        ],
        caveats: "The Connect API is Enterprise-only and not supported here — only \
                  manual exports are accepted. SRT is recommended: it preserves \
                  timestamps and speaker labels. TXT exports contain no timestamp \
                  metadata; `ts` falls back to the import date. Meeting transcripts \
                  are conversation content — this source is off by default.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["srt", "txt"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import entry point.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "srt" => import_srt(vault, path, progress),
        "txt" => import_txt(vault, path, progress),
        other => bail!("Otter importer: unsupported extension '.{other}' — drop an .srt or .txt"),
    }
}

// ---------------------------------------------------------------------------
// Content-hash guid: stable, dedup-safe, no stable id in the export file.

/// `sha256("<filename>|<content>")` truncated to 16 hex chars.
/// Scoped to filename so the same transcript text from two different meetings
/// (unlikely, but defensive) gets distinct guids. The 16-hex prefix has
/// ~2⁶⁴ collision resistance — more than sufficient for a personal vault.
fn content_guid(filename: &str, content: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(filename.as_bytes());
    h.update(b"|");
    h.update(content);
    format!("{:x}", h.finalize())[..16].to_string()
}

// ---------------------------------------------------------------------------
// Shared: load already-seen guids so re-imports skip duplicates.

fn load_seen_guids(vault: &Vault) -> Result<HashSet<String>> {
    let stream = vault.stream(CONTRACT_DIR, Partition::Month);
    let mut seen = HashSet::new();
    for key in stream.partitions()? {
        for m in stream.read::<Meeting>(&key)? {
            if !m.guid.is_empty() {
                seen.insert(m.guid);
            }
        }
    }
    Ok(seen)
}

// ---------------------------------------------------------------------------
// Shared: upsert one Meeting by guid into the contract partition.
// Re-uses the same fathom-style upsert logic (read month, merge, rewrite sorted).

fn upsert_meeting(vault: &Vault, row: Meeting) -> Result<bool> {
    let month_key = Partition::Month
        .key(&row.ts)
        .with_context(|| format!("otter: ts {:?} has no month prefix", row.ts))?
        .to_string();
    let stream = vault.stream(CONTRACT_DIR, Partition::Month);
    let mut existing: Vec<Meeting> = stream.read(&month_key)?;
    let idx = existing.iter().position(|m| m.guid == row.guid);
    let is_new = idx.is_none();
    match idx {
        Some(i) => existing[i] = row,
        None => existing.push(row),
    }
    existing.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.guid.cmp(&b.guid)));
    vault.write_snapshot(&format!("{CONTRACT_DIR}/{month_key}.jsonl"), &existing)?;
    Ok(is_new)
}

// ---------------------------------------------------------------------------
// SRT import.

/// One parsed SRT block from an Otter export.
#[derive(Debug, Clone, PartialEq)]
struct SrtBlock {
    /// Sequence number (1-based, informational only).
    seq: u32,
    /// Start time string as it appears in the SRT (e.g. `00:00:01,000`).
    start: String,
    /// End time string as it appears in the SRT.
    end: String,
    /// Text lines joined; may be prefixed `Speaker Name: text`.
    text: String,
}

/// Parse an Otter SRT body into blocks. Tolerant: skips malformed blocks
/// rather than failing the whole import. A block is:
///
/// ```text
/// <seq>
/// HH:MM:SS,mmm --> HH:MM:SS,mmm
/// [Speaker: ]text…
/// [more text lines…]
///
/// ```
fn parse_srt(body: &str) -> Vec<SrtBlock> {
    let mut blocks = Vec::new();
    // Split on blank-line separators; tolerate \r\n.
    let body = body.replace("\r\n", "\n");
    for chunk in body.split("\n\n") {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let mut lines = chunk.lines();
        // Line 1: sequence number.
        let seq: u32 = match lines.next().and_then(|l| l.trim().parse().ok()) {
            Some(n) => n,
            None => continue, // not a block
        };
        // Line 2: timecodes.
        let timecode = match lines.next() {
            Some(l) => l.trim(),
            None => continue,
        };
        let (start, end) = match parse_timecode(timecode) {
            Some(pair) => pair,
            None => continue,
        };
        // Remaining lines: text (may span multiple lines — join with space).
        let text: String = lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        blocks.push(SrtBlock { seq, start, end, text });
    }
    blocks
}

/// `HH:MM:SS,mmm --> HH:MM:SS,mmm` → (`start_str`, `end_str`).
fn parse_timecode(line: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = line.split("-->").collect();
    if parts.len() != 2 {
        return None;
    }
    let start = parts[0].trim().to_string();
    let end = parts[1].trim().to_string();
    // Basic validation: must start with HH:MM:SS
    if start.len() < 8 || end.len() < 8 {
        return None;
    }
    Some((start, end))
}

/// `HH:MM:SS,mmm` → total seconds (integer, for duration math).
fn timecode_to_secs(tc: &str) -> Option<i64> {
    // Support both `,` (SRT) and `.` (WebVTT) as millisecond separator.
    let tc = tc.replace(',', ".");
    let parts: Vec<&str> = tc.splitn(2, '.').next().unwrap_or("").split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let s: i64 = parts[2].parse().ok()?;
    Some(h * 3600 + m * 60 + s)
}

/// Optionally extract `Speaker Name` from a text line prefixed `Speaker: text`.
/// Returns `(speaker_or_empty, text_without_prefix)`.
///
/// Otter SRT exports optionally prefix text lines with the speaker name
/// followed by a colon and space: `David: Hello there.` — this is the
/// documented Otter paid-plan SRT format (speaker-labeled transcript).
fn split_speaker(text: &str) -> (String, String) {
    // Look for `Word(s): rest` — the prefix must be word characters or spaces,
    // at most 60 chars, to avoid splitting text that naturally contains colons.
    if let Some(pos) = text.find(": ") {
        let candidate = &text[..pos];
        // A speaker name: no digits, not too long, no suspicious chars.
        if candidate.len() <= 60
            && !candidate.is_empty()
            && candidate.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '\'' || c == '-' || c == '.')
        {
            return (candidate.trim().to_string(), text[pos + 2..].to_string());
        }
    }
    (String::new(), text.to_string())
}

/// Convert the first SRT timecode (`HH:MM:SS,mmm`) into an RFC3339 local
/// timestamp anchored to "today" at that offset. When the file's mtime is
/// accessible and plausible, we use it as the anchor date; otherwise we use
/// the import (Local::now) date.
///
/// This is inherently approximate for SRT imports: the file carries no
/// absolute date. The best we can do is preserve the time-of-day component
/// from the SRT and assign today's (or the mtime's) date. A future
/// DOCX/SRT-with-metadata path can supply an exact date.
fn srt_offset_to_ts(timecode: &str, anchor: &DateTime<Local>) -> String {
    let secs = timecode_to_secs(timecode).unwrap_or(0);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    use chrono::TimeZone;
    // Build a datetime from the anchor date + the SRT wall-clock time.
    // If the SRT starts at 00:00:00 (many Otter exports do), we fall back to
    // anchor's time-of-day (we can't do better without an absolute date).
    let naive = anchor.date_naive();
    let ts = Local
        .from_local_datetime(&naive.and_hms_opt(h as u32, m as u32, s as u32).unwrap_or_else(
            || naive.and_hms_opt(0, 0, 0).unwrap(),
        ))
        .earliest()
        .unwrap_or_else(|| *anchor);
    ts.to_rfc3339()
}

/// File mtime as a `DateTime<Local>`, or `None` if unavailable.
fn file_mtime(path: &Path) -> Option<DateTime<Local>> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|st| DateTime::<Local>::from(st))
}

fn import_srt(
    vault: &Vault,
    path: &Path,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let raw = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let body = String::from_utf8_lossy(&raw).into_owned();

    let filename = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("otter-export.srt");
    let guid = content_guid(filename, &raw);

    // Dedup: re-importing the same file is a no-op.
    let seen = load_seen_guids(vault)?;
    if seen.contains(&guid) {
        progress(ImportProgress { records: 0, percent: 100.0 });
        return Ok(ImportOutcome {
            headline: "0 meetings imported, 1 duplicate skipped".into(),
            counts: [("imported", 0), ("duplicates", 1), ("skipped", 0)].into(),
        });
    }

    let blocks = parse_srt(&body);
    if blocks.is_empty() {
        bail!("Otter SRT import: no transcript blocks found in {filename} — is this a valid SRT?");
    }

    // Anchor date: use the file's mtime if available; otherwise now.
    let anchor = file_mtime(path).unwrap_or_else(Local::now);

    // ts = the first block's start time, converted to RFC3339 with the mtime
    // date as anchor.
    let ts = srt_offset_to_ts(&blocks[0].start, &anchor);

    // Duration: last block's end − first block's start (whole-meeting length).
    let duration_secs = blocks.last().and_then(|last| {
        let end = timecode_to_secs(&last.end)?;
        let start = timecode_to_secs(&blocks[0].start)?;
        (end >= start).then_some(end - start)
    });

    // Build the transcript utterance sidecar (one utterance per JSONL line):
    // { "seq": N, "start": "HH:MM:SS,mmm", "end": "...", "speaker": "...", "text": "..." }
    let raw_ref = format!("{RAW_DIR}/{guid}.srt");
    let transcript_ref = format!("{RAW_DIR}/{guid}-transcript.jsonl");

    let utterances: Vec<Value> = blocks
        .iter()
        .map(|b| {
            let (speaker, text) = split_speaker(&b.text);
            let mut obj = Map::new();
            obj.insert("seq".into(), Value::from(b.seq));
            obj.insert("start".into(), Value::from(b.start.as_str()));
            obj.insert("end".into(), Value::from(b.end.as_str()));
            if !speaker.is_empty() {
                obj.insert("speaker".into(), Value::from(speaker.as_str()));
            }
            obj.insert("text".into(), Value::from(text.as_str()));
            Value::Object(obj)
        })
        .collect();

    // Write the raw verbatim SRT copy.
    let raw_path = vault.resolve(&raw_ref)?;
    if let Some(parent) = raw_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&raw_path, &raw)
        .with_context(|| format!("writing raw SRT to {raw_ref}"))?;

    // Write the utterance sidecar.
    vault.write_snapshot(&transcript_ref, &utterances)?;

    // Derive meeting title from the filename (strip extension; underscores →
    // spaces). Otter names exports like `Meeting with Alice 2026-01-15.srt`.
    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.replace('_', " ").replace('-', " ").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    let mut row = Meeting::new(SOURCE, &guid, ts.clone());
    row.started = ts;
    row.duration_secs = duration_secs;
    if !title.is_empty() {
        row.title = title;
    }
    row.transcript_ref = transcript_ref;
    row.extra.insert("srt_blocks".into(), Value::from(blocks.len() as u64));

    upsert_meeting(vault, row)?;
    progress(ImportProgress { records: 1, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "1 meeting imported ({} transcript blocks)",
            utterances.len()
        ),
        counts: [("imported", 1), ("duplicates", 0), ("skipped", 0)].into(),
    })
}

// ---------------------------------------------------------------------------
// TXT import (Needs-sample — Basic plan layout is undocumented).
//
// What we know: the TXT export is a plain text transcript with no timestamps.
// Without a real sample we can't confirm the exact speaker-label format or
// whether a meeting title/date header is present. We store the file verbatim
// in the raw layer and write a minimal contract row (ts from file mtime,
// guid from content hash, no transcript_ref since we haven't confirmed the
// sidecar shape). The raw file IS accessible via the vault for future readers.
//
// This is a raw-first / park-the-parser approach: data is preserved, the
// exact parsing is deferred until a sample is acquired.

fn import_txt(
    vault: &Vault,
    path: &Path,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let raw = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let filename = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("otter-export.txt");
    let guid = content_guid(filename, &raw);

    // Dedup: re-importing the same file is a no-op.
    let seen = load_seen_guids(vault)?;
    if seen.contains(&guid) {
        progress(ImportProgress { records: 0, percent: 100.0 });
        return Ok(ImportOutcome {
            headline: "0 meetings imported, 1 duplicate skipped".into(),
            counts: [("imported", 0), ("duplicates", 1), ("skipped", 0)].into(),
        });
    }

    // Store verbatim — never lose data, even without a parsed shape yet.
    let raw_ref = format!("{RAW_DIR}/{guid}.txt");
    let raw_path = vault.resolve(&raw_ref)?;
    if let Some(parent) = raw_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&raw_path, &raw)
        .with_context(|| format!("writing raw TXT to {raw_ref}"))?;

    // ts: file mtime, or now. TXT has no embedded timestamps.
    let anchor = file_mtime(path).unwrap_or_else(Local::now);
    let ts = anchor.to_rfc3339();

    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.replace('_', " ").replace('-', " ").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    let body = String::from_utf8_lossy(&raw);
    let line_count = body.lines().count();

    let mut row = Meeting::new(SOURCE, &guid, ts.clone());
    row.started = ts;
    if !title.is_empty() {
        row.title = title;
    }
    // No transcript_ref: TXT layout is undocumented (Needs-sample) — we store
    // the raw file but do not attempt to parse utterances.
    row.extra.insert("raw_ref".into(), Value::from(raw_ref.as_str()));
    row.extra.insert("txt_lines".into(), Value::from(line_count as u64));
    row.extra.insert("needs_sample".into(), Value::from(true));

    upsert_meeting(vault, row)?;
    progress(ImportProgress { records: 1, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!("1 meeting imported from TXT ({line_count} lines; transcript parser needs sample)"),
        counts: [("imported", 1), ("duplicates", 0), ("skipped", 0)].into(),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-otter-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(vault: &Vault, path: &std::path::Path) -> ImportOutcome {
        (IMPORT.run)(vault, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // SRT parsing unit tests.

    const SAMPLE_SRT: &str = "\
1
00:00:01,000 --> 00:00:04,000
Alice: Hello, welcome to the sync.

2
00:00:04,500 --> 00:00:08,200
Bob: Thanks Alice, let's get started.

3
00:00:08,500 --> 00:00:12,000
Alice: First, let's review the roadmap.
";

    /// SRT with no speaker labels (plain text lines only).
    const SAMPLE_SRT_NO_SPEAKER: &str = "\
1
00:00:00,000 --> 00:00:03,500
Welcome to today's meeting.

2
00:00:04,000 --> 00:00:07,000
Here is the agenda for the call.
";

    #[test]
    fn parse_srt_extracts_blocks_correctly() {
        let blocks = parse_srt(SAMPLE_SRT);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].seq, 1);
        assert_eq!(blocks[0].start, "00:00:01,000");
        assert_eq!(blocks[0].end, "00:00:04,000");
        assert_eq!(blocks[0].text, "Alice: Hello, welcome to the sync.");
        assert_eq!(blocks[1].seq, 2);
        assert_eq!(blocks[2].text, "Alice: First, let's review the roadmap.");
    }

    #[test]
    fn split_speaker_extracts_prefix_when_present() {
        let (sp, text) = split_speaker("Alice: Hello there.");
        assert_eq!(sp, "Alice");
        assert_eq!(text, "Hello there.");

        let (sp2, text2) = split_speaker("Bob Smith: Let's go.");
        assert_eq!(sp2, "Bob Smith");
        assert_eq!(text2, "Let's go.");

        // No prefix: empty speaker.
        let (sp3, text3) = split_speaker("No speaker here.");
        assert!(sp3.is_empty());
        assert_eq!(text3, "No speaker here.");

        // URL should not be treated as a speaker.
        let (sp4, text4) = split_speaker("See https://example.com for details.");
        assert!(sp4.is_empty(), "URL should not parse as speaker");
        assert_eq!(text4, "See https://example.com for details.");
    }

    #[test]
    fn split_speaker_does_not_misparse_note_colon_pattern() {
        // "Alice: Note: this is important." — only the first prefix is the speaker.
        let (sp, text) = split_speaker("Alice: Note: this is important.");
        assert_eq!(sp, "Alice");
        assert_eq!(text, "Note: this is important.");
    }

    #[test]
    fn timecode_to_secs_converts_correctly() {
        assert_eq!(timecode_to_secs("00:00:01,000"), Some(1));
        assert_eq!(timecode_to_secs("00:01:00,000"), Some(60));
        assert_eq!(timecode_to_secs("01:00:00,000"), Some(3600));
        assert_eq!(timecode_to_secs("01:23:45,678"), Some(5025));
        assert_eq!(timecode_to_secs("00:00:00,000"), Some(0));
        assert_eq!(timecode_to_secs("bad"), None);
    }

    #[test]
    fn content_guid_is_stable_and_16_hex_chars() {
        let g1 = content_guid("foo.srt", b"hello");
        let g2 = content_guid("foo.srt", b"hello");
        assert_eq!(g1, g2, "same inputs = same guid");
        assert_eq!(g1.len(), 16);
        assert!(g1.chars().all(|c| c.is_ascii_hexdigit()));

        // Different filenames → different guids even for same content.
        let g3 = content_guid("bar.srt", b"hello");
        assert_ne!(g1, g3);
    }

    #[test]
    fn parse_srt_tolerates_no_speaker_labels() {
        let blocks = parse_srt(SAMPLE_SRT_NO_SPEAKER);
        assert_eq!(blocks.len(), 2);
        let (sp, text) = split_speaker(&blocks[0].text);
        assert!(sp.is_empty());
        assert_eq!(text, "Welcome to today's meeting.");
    }

    #[test]
    fn parse_srt_skips_malformed_blocks() {
        // A body with one good block and one missing timecode.
        let bad = "1\n00:00:01,000 --> 00:00:03,000\nGood block.\n\nbad\nno timecode here\nsome text.\n\n2\n00:00:05,000 --> 00:00:07,000\nAnother good block.";
        let blocks = parse_srt(bad);
        assert_eq!(blocks.len(), 2, "only the two well-formed blocks survive");
    }

    // -----------------------------------------------------------------------
    // Full import integration tests.

    #[test]
    fn srt_import_writes_contract_raw_and_transcript_sidecar() {
        let v = temp_vault("srt_full");
        let srt_path = v.root().join("meeting-2026-06-10.srt");
        fs::write(&srt_path, SAMPLE_SRT).unwrap();

        let out = run(&v, &srt_path);
        assert_eq!(out.counts["imported"], 1);
        assert_eq!(out.counts["duplicates"], 0);

        // Contract row exists in the month partition.
        let parts = v.stream(CONTRACT_DIR, Partition::Month).partitions().unwrap();
        assert!(!parts.is_empty(), "at least one month partition");
        let rows: Vec<Meeting> = v
            .stream(CONTRACT_DIR, Partition::Month)
            .read(&parts[0])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.source, "otter");
        assert!(!m.guid.is_empty());
        assert!(!m.transcript_ref.is_empty(), "SRT import sets transcript_ref");
        assert_eq!(m.duration_secs, Some(11), "12 - 1 = 11 seconds");

        // Transcript sidecar exists with the right number of utterances.
        let sidecar = v.root().join(&m.transcript_ref);
        assert!(sidecar.exists(), "transcript sidecar written");
        let sidecar_body = fs::read_to_string(&sidecar).unwrap();
        assert_eq!(sidecar_body.lines().count(), 3, "3 utterance lines");

        // Check speaker extraction in sidecar.
        let first_line: Value =
            serde_json::from_str(sidecar_body.lines().next().unwrap()).unwrap();
        assert_eq!(
            first_line["speaker"].as_str(),
            Some("Alice"),
            "speaker label extracted from SRT prefix"
        );
        assert_eq!(
            first_line["text"].as_str(),
            Some("Hello, welcome to the sync."),
            "text without speaker prefix"
        );
        assert_eq!(first_line["start"].as_str(), Some("00:00:01,000"));

        // Raw verbatim SRT file stored.
        let raw_dir = v.root().join(RAW_DIR);
        assert!(raw_dir.exists(), "raw dir created");
        let srt_files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "srt"))
            .collect();
        assert_eq!(srt_files.len(), 1, "one raw SRT copy");
    }

    #[test]
    fn srt_import_no_speaker_labels_still_works() {
        let v = temp_vault("srt_nospk");
        let srt_path = v.root().join("plain-transcript.srt");
        fs::write(&srt_path, SAMPLE_SRT_NO_SPEAKER).unwrap();

        let out = run(&v, &srt_path);
        assert_eq!(out.counts["imported"], 1);

        let parts = v.stream(CONTRACT_DIR, Partition::Month).partitions().unwrap();
        let rows: Vec<Meeting> =
            v.stream(CONTRACT_DIR, Partition::Month).read(&parts[0]).unwrap();
        let m = &rows[0];
        // Sidecar exists; no speaker key expected in blocks.
        let sidecar = fs::read_to_string(v.root().join(&m.transcript_ref)).unwrap();
        let first: Value = serde_json::from_str(sidecar.lines().next().unwrap()).unwrap();
        assert!(
            first.get("speaker").is_none(),
            "no speaker key when no speaker label present"
        );
    }

    #[test]
    fn srt_import_deduplicates_on_re_import() {
        let v = temp_vault("srt_dedup");
        let srt_path = v.root().join("meeting-dup.srt");
        fs::write(&srt_path, SAMPLE_SRT).unwrap();

        let out1 = run(&v, &srt_path);
        assert_eq!(out1.counts["imported"], 1);

        // Re-import the same file.
        let out2 = run(&v, &srt_path);
        assert_eq!(out2.counts["duplicates"], 1, "same file = duplicate, skipped");
        assert_eq!(out2.counts["imported"], 0);

        // Only one row in the vault.
        let parts = v.stream(CONTRACT_DIR, Partition::Month).partitions().unwrap();
        let rows: Vec<Meeting> =
            v.stream(CONTRACT_DIR, Partition::Month).read(&parts[0]).unwrap();
        assert_eq!(rows.len(), 1, "still exactly one contract row");
    }

    #[test]
    fn srt_import_two_different_files_get_distinct_guids() {
        let v = temp_vault("srt_two");
        let p1 = v.root().join("meeting-a.srt");
        let p2 = v.root().join("meeting-b.srt");
        fs::write(&p1, SAMPLE_SRT).unwrap();
        fs::write(&p2, SAMPLE_SRT_NO_SPEAKER).unwrap();

        run(&v, &p1);
        run(&v, &p2);

        // Both imported, distinct guids.
        let mut all_rows: Vec<Meeting> = Vec::new();
        for key in v.stream(CONTRACT_DIR, Partition::Month).partitions().unwrap() {
            all_rows.extend(
                v.stream(CONTRACT_DIR, Partition::Month).read::<Meeting>(&key).unwrap(),
            );
        }
        assert_eq!(all_rows.len(), 2, "two distinct meetings");
        assert_ne!(all_rows[0].guid, all_rows[1].guid, "distinct guids");
    }

    #[test]
    fn txt_import_stores_raw_and_writes_contract_row_no_transcript_ref() {
        let v = temp_vault("txt_full");
        let txt_path = v.root().join("meeting-notes.txt");
        let txt_body = "Alice: Welcome to today's meeting.\nBob: Thanks for having me.\n";
        fs::write(&txt_path, txt_body).unwrap();

        let out = run(&v, &txt_path);
        assert_eq!(out.counts["imported"], 1);

        let parts = v.stream(CONTRACT_DIR, Partition::Month).partitions().unwrap();
        let rows: Vec<Meeting> =
            v.stream(CONTRACT_DIR, Partition::Month).read(&parts[0]).unwrap();
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.source, "otter");
        assert!(m.transcript_ref.is_empty(), "TXT import has no transcript_ref (Needs-sample)");
        assert!(
            m.extra.get("needs_sample").and_then(Value::as_bool).unwrap_or(false),
            "needs_sample flag in extra"
        );
        assert!(
            m.extra.contains_key("raw_ref"),
            "raw_ref preserved in extra for future readers"
        );

        // Raw file stored verbatim.
        let raw_ref = m.extra["raw_ref"].as_str().unwrap();
        let raw_on_disk = fs::read_to_string(v.root().join(raw_ref)).unwrap();
        assert_eq!(raw_on_disk, txt_body);
    }

    #[test]
    fn txt_import_deduplicates_on_re_import() {
        let v = temp_vault("txt_dedup");
        let txt_path = v.root().join("dup-notes.txt");
        fs::write(&txt_path, "Hello meeting notes.\n").unwrap();

        let out1 = run(&v, &txt_path);
        assert_eq!(out1.counts["imported"], 1);

        let out2 = run(&v, &txt_path);
        assert_eq!(out2.counts["duplicates"], 1);

        let parts = v.stream(CONTRACT_DIR, Partition::Month).partitions().unwrap();
        let rows: Vec<Meeting> =
            v.stream(CONTRACT_DIR, Partition::Month).read(&parts[0]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn unsupported_extension_returns_error() {
        let v = temp_vault("bad_ext");
        let bad_path = v.root().join("transcript.docx");
        fs::write(&bad_path, b"fake docx content").unwrap();

        let result = (IMPORT.run)(&v, &bad_path, &BTreeMap::new(), &mut |_| {});
        assert!(result.is_err(), "unsupported .docx returns error");
        assert!(result.unwrap_err().to_string().contains("docx"));
    }
}
