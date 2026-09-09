# Webex

- **id:** `webex`
- **domains:** `meetings` (contract: **not yet ratified** — Phase 3 drafts it
  from Granola + Fathom + Zoom + Fireflies together; Webex joins it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll `meetingTranscripts` list; watermark cursor)
- **connection:** `webex` — OAuth (`meeting:read` scope; free and paid Webex
  accounts both supported). Single def on this connection. Auth URL:
  `https://webexapis.com/v1/authorize`, token URL: `https://webexapis.com/v1/access_token`,
  redirect port 38854 (assigned).
- **evidence:** official-docs — developer.webex.com (REST API:
  `GET /v1/meetingTranscripts` list + per-transcript VTT/TXT download links)
- **effort / priority:** M / P2
- **needs:** privacy-sensitive (meeting transcripts = conversation content —
  opt-in with explicit acknowledgement) · Needs-David (contract: meetings) ·
  Needs-login (validation only — David has no Webex account; any real user's
  run can validate)

## What it is

Cisco's enterprise meeting platform. Primarily corporate use, but free
personal accounts exist and the transcript API covers both. For users whose
work life runs through Webex, this is the only way meeting content reaches
the vault — same value proposition as Zoom/Granola, smaller personal user
base (hence P2, after Zoom and Google Meet).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transcript list | free + paid accounts | meeting id, title, time, transcript ids | official docs |
| Transcript content | meetings where the assistant transcribed | VTT or TXT download (speaker-attributed utterances) | official docs |
| AI summaries / recordings (MP4) | account-dependent | summary text, recording files | official docs (noted; recordings likely out of scope — transcripts first) |

As of 2026 the API returns both Webex Assistant and Cisco AI Assistant
transcripts. All fields optional in the contract; no tier-specific code
paths.

## Access & auth

- REST: `GET /v1/meetingTranscripts` (list), then
  `vttDownloadLink`/`txtDownloadLink` per transcript. OAuth 2.0
  (`meeting:read`); Personal Access Tokens exist but expire — OAuth is the
  shippable path (ConnectSpec: baked + BYO creds).
- **Download links expire** — never persist them; re-fetch from the list
  endpoint each pull.
- **Recurring meeting series:** transcripts hang off the *instance* ID, not
  the parent series ID — enumerate instances.
- No TCC, no local files. Standalone-clean (plain HTTPS poll; no webhooks
  needed).

## Vault mapping

- **Raw layer:** `meetings/webex/raw/YYYY-MM.jsonl` — list-endpoint objects,
  full fidelity; VTT sidecars at `meetings/webex/transcripts/<id>.vtt`.
- **Contract layer:** `meetings/webex/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per meeting (`ts`, `source`, `guid` =
  transcript/meeting id, `title`, `duration_secs`, `attendees[]`,
  `summary`), normalized plain-text transcript in the row for search,
  rich VTT preserved as sidecar, overflow in `extra`.
- **Dedupe:** meeting-instance id as `guid`; cursor in
  `.trove/webex-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/webex.rs`: `DEF` (Periodic, slow tick),
   `CONNECTION` (OAuth via the generic flow in `sync/oauth.rs`; BYO
   client-id fallback per docs/oauth-distribution.md), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Reuse the shared VTT parser built for Zoom (same WebVTT shape as
   Zoom/Teams) — don't write a second one.
4. Fixtures from developer.webex.com example responses (list + VTT); tests
   for expired-link refetch and series-instance enumeration; unique temp
   dirs.
5. Privacy: transcript collection is opt-in with explicit acknowledgement
   (conversation content); the toggle copy says so.
6. Bound to the ratified `meetings` contract (reuse-bound, same shape as
   Zoom) — no longer Needs-David on the contract. Remaining David dependency
   is **live-account validation** (David has no Webex account; any real
   user's run validates). Built after Zoom so the VTT path is exercised.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Transcript list + download | ✅ built, awaiting live test | connect a real Webex account that has at least one assistant-transcribed meeting; Sync now; confirm rows in `meetings/webex/` + VTT sidecar + hub last-data |
| Recurring-series instances | ✅ built | same account with a recurring transcribed meeting; confirm per-instance rows, no parent-series misses (each transcript's `id` is stable and per-instance)  |

## Build notes (2026-06-21)

- Module: `crates/trove-core/src/webex.rs` — full OAuth + Periodic pull.
- API shape confirmed from `api-evangelist/webex` example JSON + Python SDK (`wxc_sdk`):
  fields `id`, `startTime`, `meetingId`, `meetingTopic`, `siteUrl`, `scheduledMeetingId`,
  `meetingSeriesId`, `hostUserId`, `vttDownloadLink`, `txtDownloadLink`, `status`.
- Pagination via RFC 5988 `Link: <url>; rel="next"` response header (confirmed from `wxc_sdk`).
- VTT download links expire — never persisted; re-fetched each poll.
- CONTRACT rows: `guid` = transcript `id`; `platform` = "webex"; source-specific fields → `extra`.
- Raw sidecar: the raw VTT bytes at `meetings/webex/raw/transcripts/<id>.vtt`.
- 10 unit tests; all green. No new crate deps.

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Webex
(L610-617; at-a-glance L484). Feasibility 🟢 high for Webex users.
Cross-cutting notes: shared meetings vault shape (note 1), OAuth pattern
reuse (note 2), poll-don't-webhook (note 3), VTT normalization + sidecar
pattern (note 4). Not time-sensitive — transcripts persist server-side.
Sequenced after Zoom/Google Meet on user-base size, not difficulty.
