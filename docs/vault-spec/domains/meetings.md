# Domain: meetings

Every meeting an AI notetaker or platform captured — one **record per
meeting**, not per utterance. AI notetakers (Granola, Fathom, Fireflies,
Otter, Read.ai, tl;dv, Krisp) and meeting platforms (Zoom, Google Meet,
Webex) all write this shape: a notetaker that joined a Zoom call and Zoom's
own cloud recording of it each land a row in their own folder, and the
reader merges them at read time (a meeting seen by two tools is two rows
with two `guid`s until the read-time layer reconciles them). Meetings do
**not** route to `correspondence/` — one meeting is not one message, and an
utterance flood does not belong in the message timeline.

- **Layout:** `meetings/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/meetings.meeting.schema.json`](../schemas/meetings.meeting.schema.json)
- **Dedupe key:** `guid` (source-unique: Granola note id, Fathom/Read.ai/tl;dv
  meeting id, Fireflies transcript id, Zoom meeting UUID, Meet conference
  record id, Webex instance id, a content hash for export imports). Imports
  must skip already-stored guids; a transcript that arrives on a later poll
  upserts the same row.

## The meeting

One line per meeting. Only `ts` / `source` / `guid` are required — those
three place and identify the record. A rich API source (Granola, Fireflies)
fills most fields; a sparse export import (an Otter or Krisp `.txt` with no
embedded id) writes little more than the core, deriving `ts` from an SRT
timestamp or the file's date and a content-hash `guid`. Tiered capabilities
need no special fields: a free-tier row simply carries no `summary` or
`transcript_ref`.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the meeting started (= `started` when known; falls back to the export's or file's date for sources with no start time) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique meeting id, the dedupe key |
| `title` | string | | meeting title / topic (export imports may carry none) |
| `started`, `ended` | string | | RFC3339 local meeting bounds, when the source reports them |
| `duration_secs` | int | | meeting length in seconds |
| `platform` | string | | conferencing platform the meeting ran on: `"zoom"` \| `"meet"` \| `"teams"` \| `"webex"` \| … (the recorder's own brand, verbatim, when that's all that's known) |
| `attendees` | string[] | | raw handles — lowercased emails / service ids (not display names) |
| `attendee_names` | string[] | | display names, positionally paired with `attendees` where the source gives both |
| `host` | string | | organizer/host handle (same shaping as `attendees`) |
| `summary` | string | | AI summary / notes, markdown (omitted by sources that yield only transcripts, e.g. Meet) |
| `meeting_url` | string | | the join/conference URL |
| `recording_url` | string | | link to the recording, when the source exposes a durable one |
| `folder` | string | | the notetaker's own folder / workspace label for the meeting |
| `transcript_ref` | string | | vault-relative path to the transcript sidecar (the source's raw file or a copied artifact); omitted when no transcript exists |
| `extra` | object | | everything source-specific (action items, keywords, speaker analytics, sentence-level AI tags, Drive Doc refs, …) |

Emit `attendee_names` only when it is fully aligned with `attendees` — same
length and same order; if a source has names for only some attendees, put them
in `extra` rather than write a misaligned `attendee_names`.

Omit empty fields. Unknown fields are tolerated.

**Transcripts are sidecar artifacts, never inlined.** The contract row does
not carry an utterances array — full transcripts (speaker-labeled,
timestamped sentences, VTT, SRT) live in the source's own raw folder
(`meetings/<source>/raw/…`), and `transcript_ref` points at them when a
reader wants the text. The contract is the per-meeting convergence; the raw
layer keeps every utterance at full fidelity.

## Examples

```jsonl
{"ts":"2026-06-10T09:00:00-07:00","source":"granola","guid":"note_8f2a1c","title":"Q3 Roadmap Sync","duration_secs":2940,"started":"2026-06-10T09:00:00-07:00","ended":"2026-06-10T09:49:00-07:00","platform":"zoom","attendees":["dwills@example.com","sam@example.com","ana@example.com"],"attendee_names":["David Wills","Sam Ortiz","Ana"],"host":"dwills@example.com","summary":"## Decisions\n- Ship the meetings contract first\n- Sam owns the Fireflies validation pass\n\n## Action items\n- [ ] Ana to draft the Q3 deck","folder":"Work","meeting_url":"https://zoom.us/j/123456789","transcript_ref":"meetings/granola/raw/files/note_8f2a1c.jsonl","extra":{"workspace":"Work","creator_email":"dwills@example.com"}}
{"ts":"2026-05-28T14:30:00-04:00","source":"fireflies","guid":"01HXYZ7Q8K","title":"Customer Discovery — Acme","attendees":["jordan@acme.com","dwills@example.com"],"platform":"meet","summary":"Acme wants SSO before they expand. Pricing follow-up next week.","transcript_ref":"meetings/fireflies/raw/2026-05.jsonl","extra":{"action_items":["Send SOC2 report","Schedule pricing call"],"keywords":["SSO","pricing","expansion"]}}
{"ts":"2026-06-02T11:05:00-07:00","source":"otter","guid":"sha256-3b9d4e7a"}
```

## Read-time semantics (FYI for writers)

The meetings reader scans `meetings/*/` — creating your source folder is the
registration. Two sources that captured the same meeting (a notetaker plus
the platform's own recording) each keep their row and `guid`; precedence and
cross-source dedupe are a read-time opinion, never a write-time merge. Write
raw handles in `attendees`/`host`, not resolved people — a contacts source
maps them later. Keep transcripts in the raw folder and reference them by
`transcript_ref`; never inline an utterance stream into the row. The
read-time layer may surface meetings alongside `correspondence/` and
`calendar/` in a unified "conversations" view — that is a derived join, not a
reason to reshape what you write.
