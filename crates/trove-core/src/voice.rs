//! The `voice` domain contract: spoken audio the user holds — self-recorded
//! voice memos and received voicemails — in one normalized, source-agnostic
//! event stream.
//!
//! Each source writes full-fidelity rows under `voice/<source>/YYYY-MM.jsonl`
//! (`<source>` is the collector id and the folder name; the month is the
//! month of [`Recording::ts`]). The stream is **append-only** — a clip
//! happens once, like a [`crate::correspondence::Message`] — and imports skip
//! guids they already hold ([`Recording::guid`] is the dedupe key). Apple
//! Voice Memos, Visual Voicemail, and Google Voice voicemails all converge
//! here; a reader scans `voice/*/` and sees one spoken-artifact stream, with
//! [`Recording::kind`] separating a memo (no caller) from a voicemail (a
//! caller handle in [`Recording::sender`]).
//!
//! **Audio is never copied into the vault.** [`Recording::audio_ref`] points
//! at the source clip — the vault holds the metadata and transcript, the
//! player follows the reference (the "files are the source of truth, don't
//! duplicate" rule). Whatever a source carries that the normalized columns
//! don't map — memo folder, voicemail read/trashed flags, per-word
//! confidences — rides verbatim in [`Recording::extra`].
//!
//! See [`docs/vault-spec/domains/voice.md`] for the field-level spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One spoken clip — one line of `voice/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/
/// `kind` are required; everything else is omit-empty, so a just-recorded
/// untitled memo or a pre-transcript voicemail writes a minimal line. The
/// core columns are what the read-time person graph and any reader key on;
/// everything source-specific they don't carry is preserved verbatim under
/// [`extra`](Recording::extra) rather than dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Recording {
    /// RFC3339 local time the clip was recorded / the voicemail was left.
    /// Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`apple-voice-memos`,
    /// `apple-voicemail`, `google-voice`). Always serialized.
    pub source: String,
    /// `"memo"` (self-recording) | `"voicemail"` (received). Always
    /// serialized — the convergence's discriminator.
    pub kind: String,
    /// User/Apple-given memo title; voicemails rarely have one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Clip length, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
    /// Caller handle, voicemails only — E.164 where parseable, else as the
    /// source gave it. Absent on memos (a memo has no caller).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sender: String,
    /// Display name for the caller, when the source supplies one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sender_name: String,
    /// Machine transcript text (Apple `tsrp` atom / voicemail PLIST / Google
    /// Voice HTML), full fidelity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transcript: String,
    /// Path to the source audio (`.m4a`/`.amr`/`.mp3`); never inline audio
    /// bytes. The contract describes this as vault-relative, but local
    /// collectors that must not duplicate the user's audio (Apple Voice
    /// Memos) write the original absolute path here instead — see
    /// [`crate::apple_voice_memos`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audio_ref: String,
    /// Source-unique id, the dedupe key (Voice Memos recording UUID,
    /// device-UUID + voicemail rowid, a stable hash of the Google Voice
    /// conversation).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
    /// Everything source-specific the normalized fields don't carry — memo
    /// folder, voicemail read/trashed flags, per-word confidences — full
    /// fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Recording {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, kind: impl Into<String>, ts: impl Into<String>) -> Self {
        Recording {
            ts: ts.into(),
            source: source.into(),
            kind: kind.into(),
            title: String::new(),
            duration_secs: None,
            sender: String::new(),
            sender_name: String::new(),
            transcript: String::new(),
            audio_ref: String::new(),
            guid: String::new(),
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
        let r = Recording::new("apple-voice-memos", "memo", "2026-06-10T07:42:13-07:00");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"ts": "2026-06-10T07:42:13-07:00", "source": "apple-voice-memos", "kind": "memo"})
        );
    }

    #[test]
    fn full_record_round_trips() {
        let line = json!({
            "ts": "2026-05-22T18:05:00-07:00",
            "source": "google-voice",
            "kind": "voicemail",
            "duration_secs": 27,
            "sender": "+14155550137",
            "sender_name": "Dr. Reyes Office",
            "transcript": "Hi, this is Dr. Reyes's office confirming your appointment on Friday at ten.",
            "audio_ref": "voice/google-voice/audio/2026-05-22T180500Z_+14155550137.mp3",
            "guid": "gv-vm-8f3c1a2b"
        });
        let r: Recording = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(r.kind, "voicemail");
        assert_eq!(r.sender, "+14155550137");
        assert_eq!(r.duration_secs, Some(27));
        assert_eq!(serde_json::to_value(&r).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_extra_round_trips() {
        // Forward-compat: an unknown top-level field is ignored; `extra`
        // carries source-specific bag verbatim.
        let line = json!({
            "ts": "2026-06-10T07:42:13-07:00",
            "source": "apple-voice-memos",
            "kind": "memo",
            "title": "Standup ideas",
            "future_field": "ignored",
            "extra": {"folder": "All Recordings", "read": false}
        });
        let r: Recording = serde_json::from_value(line).unwrap();
        assert_eq!(r.title, "Standup ideas");
        assert_eq!(r.extra.get("folder"), Some(&Value::String("All Recordings".into())));
        // Re-serialized form drops the unknown field but keeps extra.
        let re = serde_json::to_value(&r).unwrap();
        assert!(re.get("future_field").is_none());
        assert_eq!(re["extra"]["read"], json!(false));
    }
}
