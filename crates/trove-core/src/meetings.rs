//! The `meetings` domain contract: one **record per meeting** an AI notetaker
//! or platform captured — never one per utterance. AI notetakers (Granola,
//! Fathom, Fireflies, Otter, Read.ai, tl;dv, Krisp) and meeting platforms
//! (Zoom, Google Meet, Webex) all converge on this shape: a notetaker that
//! joined a Zoom call and Zoom's own cloud recording of it each land a row in
//! their own folder, and the reader reconciles them at read time.
//!
//! Each source writes full-fidelity rows under `meetings/<source>/YYYY-MM.jsonl`
//! (`<source>` is the collector id and folder name; the month is the month of
//! [`Meeting::ts`]). The stream is **append-only** — a meeting happens once —
//! and [`Meeting::guid`] is the dedupe key (a source-unique meeting id). One
//! behavioral wrinkle the readers and writers share: a transcript that arrives
//! on a *later* poll **upserts the same `guid` row in place** (to set
//! [`Meeting::transcript_ref`]) rather than appending a duplicate.
//!
//! **Transcripts are sidecar artifacts, never inlined.** The contract row
//! carries no utterances array — full transcripts (speaker-labeled, timestamped
//! sentences) live in the source's own raw folder (`meetings/<source>/raw/…`),
//! and [`Meeting::transcript_ref`] points at them when a reader wants the text.
//! Meetings do **not** route to `correspondence/` — one meeting is not one
//! message, and an utterance flood does not belong in the message timeline.
//!
//! See [`docs/vault-spec/domains/meetings.md`] for the field-level spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One meeting — one line of `meetings/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`guid`
/// are required — those three place and identify the record; everything else is
/// omit-empty, so a sparse export import (an Otter/Krisp `.txt` with no embedded
/// id) writes little more than the core, while a rich API source (Granola,
/// Fathom, Fireflies) fills most fields. Source-specific data the normalized
/// columns don't carry (action items, keywords, speaker analytics, highlights)
/// is preserved verbatim under [`extra`](Meeting::extra) rather than dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Meeting {
    /// RFC3339 local time the meeting started (= [`started`](Meeting::started)
    /// when known; falls back to the export's or file's date for sources with
    /// no start time). Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`granola`, `fathom`,
    /// `fireflies`). Always serialized.
    pub source: String,
    /// Source-unique meeting id, the dedupe key (Granola note id,
    /// Fathom/Read.ai/tl;dv meeting id, Fireflies transcript id, Zoom meeting
    /// UUID, Meet conference record id, Webex instance id, a content hash for
    /// export imports). Always serialized.
    pub guid: String,
    /// Meeting title / topic (export imports may carry none).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// RFC3339 local meeting start, when the source reports it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub started: String,
    /// RFC3339 local meeting end, when the source reports it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ended: String,
    /// Meeting length, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
    /// Conferencing platform the meeting ran on: `"zoom"` | `"meet"` |
    /// `"teams"` | `"webex"` | … (the recorder's own brand, verbatim, when
    /// that's all that's known).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub platform: String,
    /// Raw handles — lowercased emails / service ids (not display names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
    /// Display names, positionally paired with [`attendees`](Meeting::attendees)
    /// where the source gives both. Emit only when fully aligned — same length
    /// and order; a source with names for only some attendees puts them in
    /// [`extra`](Meeting::extra) rather than write a misaligned array.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendee_names: Vec<String>,
    /// Organizer/host handle (same shaping as [`attendees`](Meeting::attendees)).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// AI summary / notes, markdown (omitted by sources that yield only
    /// transcripts, e.g. Meet).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// The join/conference URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub meeting_url: String,
    /// Link to the recording, when the source exposes a durable one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recording_url: String,
    /// The notetaker's own folder / workspace label for the meeting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub folder: String,
    /// Vault-relative path to the transcript sidecar (the source's raw file or
    /// a copied artifact); omitted when no transcript exists yet. A transcript
    /// that arrives on a later poll sets this on the existing `guid` row.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transcript_ref: String,
    /// Everything source-specific the normalized fields don't carry — action
    /// items, keywords, speaker analytics, sentence-level AI tags, highlights,
    /// Drive Doc refs — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Meeting {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, guid: impl Into<String>, ts: impl Into<String>) -> Self {
        Meeting {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            title: String::new(),
            started: String::new(),
            ended: String::new(),
            duration_secs: None,
            platform: String::new(),
            attendees: Vec::new(),
            attendee_names: Vec::new(),
            host: String::new(),
            summary: String::new(),
            meeting_url: String::new(),
            recording_url: String::new(),
            folder: String::new(),
            transcript_ref: String::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_record_serializes_only_required_fields() {
        let m = Meeting::new("otter", "sha256-3b9d4e7a", "2026-06-02T11:05:00-07:00");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"ts": "2026-06-02T11:05:00-07:00", "source": "otter", "guid": "sha256-3b9d4e7a"})
        );
    }

    #[test]
    fn full_record_round_trips() {
        let line = json!({
            "ts": "2026-06-10T09:00:00-07:00",
            "source": "granola",
            "guid": "note_8f2a1c",
            "title": "Q3 Roadmap Sync",
            "duration_secs": 2940,
            "started": "2026-06-10T09:00:00-07:00",
            "ended": "2026-06-10T09:49:00-07:00",
            "platform": "zoom",
            "attendees": ["dwills@example.com", "sam@example.com", "ana@example.com"],
            "attendee_names": ["David Wills", "Sam Ortiz", "Ana"],
            "host": "dwills@example.com",
            "summary": "## Decisions\n- Ship the meetings contract first",
            "meeting_url": "https://zoom.us/j/123456789",
            "transcript_ref": "meetings/granola/raw/files/note_8f2a1c.jsonl",
            "extra": {"workspace": "Work"}
        });
        let m: Meeting = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(m.title, "Q3 Roadmap Sync");
        assert_eq!(m.duration_secs, Some(2940));
        assert_eq!(m.attendees.len(), 3);
        assert_eq!(m.attendee_names.len(), 3);
        assert_eq!(m.transcript_ref, "meetings/granola/raw/files/note_8f2a1c.jsonl");
        assert_eq!(serde_json::to_value(&m).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_extra_round_trips() {
        // Forward-compat: an unknown top-level field is ignored; `extra` carries
        // the source-specific bag verbatim.
        let line = json!({
            "ts": "2026-05-28T14:30:00-04:00",
            "source": "fireflies",
            "guid": "01HXYZ7Q8K",
            "title": "Customer Discovery — Acme",
            "future_field": "ignored",
            "extra": {"action_items": ["Send SOC2 report"], "keywords": ["SSO"]}
        });
        let m: Meeting = serde_json::from_value(line).unwrap();
        assert_eq!(m.title, "Customer Discovery — Acme");
        assert_eq!(m.extra.get("keywords"), Some(&json!(["SSO"])));
        // Re-serialized form drops the unknown field but keeps extra.
        let re = serde_json::to_value(&m).unwrap();
        assert!(re.get("future_field").is_none());
        assert_eq!(re["extra"]["action_items"], json!(["Send SOC2 report"]));
    }
}
