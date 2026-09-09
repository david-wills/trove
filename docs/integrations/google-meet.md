# Google Meet

- **id:** `google-meet`
- **domains:** `meetings` (contract: **not yet ratified** — Phase 3 drafts it
  from Granola + Fathom + Zoom + Fireflies together; Meet conforms once
  ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll conference records; **time-sensitive** — the
  API deletes transcript entries 30 days after the conference ends)
- **connection:** `google` — OAuth (existing connection; shared with the six
  shipped Google defs). Adds the `meetings.space.readonly` scope to the
  full-scope bundle.
- **evidence:** official-docs — Google Meet REST API v2
  (`conferenceRecords`, `…/transcripts`, `…/transcripts/{id}/entries`)
- **effort / priority:** M / P2
- **needs:** Privacy-sensitive (opt-in) · Needs-David (contract: meetings;
  google scope-bundle change touches the shipped consent flow)

## What it is

Google's meeting platform. When the organizer enables transcription, Meet
produces utterance-level transcripts retrievable via the Meet REST API v2 and
a durable Google Doc copy in the organizer's Drive. Covers Workspace *and*
personal @gmail.com accounts. The catch: transcription is off by default,
non-organizers can't reach recordings (Drive permissions apply), and API
transcript entries expire after 30 days.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Conference records | any Google account | meeting space, start/end times | official API v2 docs |
| Transcript entries | organizer enabled transcription; ≤30 days old | utterances w/ speakers | official API v2 docs |
| Durable Drive Doc | organizer's Drive (permission-gated) | transcript doc via `exportUri` | official API v2 docs |

All optional in the contract; a meeting with no transcript still yields a
metadata row. No tier-specific code paths.

## Access & auth

- Meet REST API v2: `GET /v2/conferenceRecords`,
  `GET /v2/conferenceRecords/{id}/transcripts`,
  `GET /v2/conferenceRecords/{id}/transcripts/{id}/entries`. The
  `DocsDestination.exportUri` gives the Drive Doc download for the durable
  copy.
- OAuth scope `https://www.googleapis.com/auth/meetings.space.readonly` on
  the existing `google` connection (multi-account by `sub`, full-scope-bundle
  consent — adding a scope means re-consent for connected accounts).
- Recordings live in the *organizer's* Drive; OAuth does not override Drive
  sharing — non-organizer pulls will legitimately come back empty.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `meetings/google-meet/raw/YYYY-MM.jsonl` — conference
  records + transcript entries, full fidelity, partitioned by account `sub`
  convention matching the other Google defs.
- **Contract layer:** `meetings/google-meet/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per conference (`ts`, `source`, `guid` =
  conference record name/id, `title`, `duration_secs`, `summary` absent —
  Meet yields transcripts, not AI summaries); transcripts as sidecar
  documents; Drive Doc reference in `extra`.
- **Dedupe:** conference record id as `guid`; cursor in
  `.trove/google-meet-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/google_meet.rs`: `DEF` (Periodic, short
   cadence — the 30-day entry expiry makes prompt pulls matter),
   `connection: Some("google")`, `pull` hook.
2. One registration line in `INTEGRATIONS`; no new connection.
3. Extend the google scope bundle with `meetings.space.readonly`; handle
   already-connected accounts needing re-consent (surface in the connect
   card, per the disabled-controls-affordance rule).
4. Fetch entries first; fall back to the Drive Doc via `exportUri` for
   conferences older than the 30-day window.
5. Fixtures from the API v2 reference examples; tests for the empty-
   transcript and expired-entries cases. Privacy: default-off, explicit
   opt-in (transcript bodies).
6. Parked behind Needs-David (meetings contract) until ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Conference records | ✅ built | connect a Google account (after scope added), hold a Meet call, Sync now; confirm metadata row in `meetings/google-meet/YYYY-MM.jsonl` |
| Transcript entries | ✅ built | organizer-enable transcription in a test meeting; confirm utterances sidecar in `meetings/google-meet/raw/transcripts/<conf_id>.jsonl` within 30 days |
| Drive Doc fallback | ✅ built | `_docsExportUri` is the first line in the transcript sidecar; Drive Doc is not re-fetched (requires Drive permission), link is stored for the reader |

**Blocker (Needs-David):** The `meetings.space.readonly` OAuth scope is not yet in the shared Google scope bundle (`sync/google.rs` `SCOPES`). Until David adds it and connected accounts re-consent, `conferenceRecords.list` returns an empty list or 403 — the pull degrades gracefully to a no-op.

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Google
Meet Recordings & Transcripts (L554–L561). Feasibility 🟡 medium: manual
per-meeting transcription enablement, 30-day API retention (the
time-sensitivity flag), organizer-gated recordings. Research recommends
pairing with a Google Drive integration for the durable Doc; the shipped
`google` connection provides the auth scaffolding. P2 behind the
token-paste notetakers, which exercise the contract more cheaply.
