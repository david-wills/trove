# Domain: voice

Spoken audio items the user holds: self-recorded voice memos and received
voicemails, each a timestamped clip with optional machine transcript. Apple
Voice Memos, Visual Voicemail (from a local iPhone backup), and Google Voice
voicemails all write this shape; readers see one stream of spoken artifacts,
with `kind` separating a memo (no caller) from a voicemail (a caller handle).
Audio is never copied into the vault — each row points at the source file by
relative path. Calls and SMS are *not* here: Google Voice's call log and
texts route to `correspondence/` (its voicemails alone land here).

- **Layout:** `voice/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/voice.recording.schema.json`](../schemas/voice.recording.schema.json)
- **Dedupe key:** `guid` (source-unique: Voice Memos recording UUID,
  device-UUID + voicemail rowid, a stable hash of the Google Voice
  conversation). Imports must skip already-stored guids.

## Recording

Only `ts`/`source`/`kind` are required; everything else is omit-empty, so a
just-recorded untitled memo or a pre-transcript voicemail writes a minimal
line. `sender` is present on voicemails and absent on memos (a memo has no
caller) — it is the convergence's only kind-specific field.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the clip was recorded / the voicemail was left |
| `source` | string | ✔ | collector id, = the folder name |
| `kind` | string | ✔ | `"memo"` (self-recording) \| `"voicemail"` (received) |
| `title` | string | | user/Apple-given memo title; voicemails rarely have one |
| `duration_secs` | int | | clip length in seconds |
| `sender` | string | | caller handle, voicemails only — E.164 where parseable, else as the source gave it |
| `sender_name` | string | | display name for the caller, when the source supplies one |
| `transcript` | string | | machine transcript text (Apple `tsrp` atom / voicemail PLIST / Google Voice HTML), full fidelity |
| `audio_ref` | string | | vault-relative path to the source audio (`.m4a`/`.amr`/`.mp3`); never inline audio bytes |
| `guid` | string | | source-unique id, the dedupe key |
| `extra` | object | | everything source-specific (memo folder, voicemail read/trashed flags, per-word confidences, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T07:42:13-07:00","source":"apple-voice-memos","kind":"memo","title":"Standup ideas","duration_secs":83,"transcript":"Remember to raise the caching question and push the demo to Thursday.","audio_ref":"voice/apple-voice-memos/audio/20260610 074213.m4a","guid":"A1B2C3D4-0001-4F00-9ABC-0123456789AB"}
{"ts":"2026-05-22T18:05:00-07:00","source":"google-voice","kind":"voicemail","duration_secs":27,"sender":"+14155550137","sender_name":"Dr. Reyes Office","transcript":"Hi, this is Dr. Reyes's office confirming your appointment on Friday at ten.","audio_ref":"voice/google-voice/audio/2026-05-22T180500Z_+14155550137.mp3","guid":"gv-vm-8f3c1a2b"}
{"ts":"2026-05-30T12:18:44-07:00","source":"apple-voicemail","kind":"voicemail","duration_secs":41,"sender":"+442071838750","audio_ref":"voice/apple-voicemail/audio/00000042.amr","guid":"3f9ab1c2-42"}
```

## Read-time semantics (FYI for writers)

The voice reader scans `voice/*/` — creating your source folder is the
registration. Memos and voicemails sit in one spoken-artifact stream;
`kind` separates them, and `sender` lets the read-time person graph join a
voicemail caller to the same handle seen in `correspondence/` calls. Audio
stays at its `audio_ref` path — the vault holds the metadata and transcript,
the player follows the reference. Transcripts and caller handles are
privacy-sensitive: these sources ship opt-in with explicit acknowledgement,
but a written row is full-fidelity and never trimmed at write time.
