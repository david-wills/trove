# Granola

- **id:** `granola`
- **domains:** `meetings/` (contract: **Phase 3 pending** — drafted from
  Granola + Fathom + Zoom + Fireflies together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll for new notes; watermark cursor)
- **connection:** `granola` — TokenPaste (API key from Granola settings; no
  OAuth dance). Not shared with other defs.
- **evidence:** official-docs — docs.granola.ai (REST API, example responses,
  documented rate limits); official + community MCP servers exist
- **effort / priority:** S / P1
- **needs:** privacy (meeting content ≈ message bodies — opt-in with explicit
  acknowledgement) · Needs-login (validation only — build proceeds from
  documented shapes) · meetings contract not yet ratified (Needs-David)

## What it is

AI meeting-notes app: joins/listens to meetings and produces AI summaries,
and (on paid plans) full transcripts with speaker attribution. High-value —
meeting content is otherwise never captured locally. Well-funded ($125M
raised March 2026 at $1.5B valuation per the research doc) — unlikely
shutdown.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Meeting metadata | Business+ (API key required) | title, time, attendees | official docs |
| AI summary/notes | Business+ (API key required) | summary markdown | official docs |
| Full transcript | Business+ | utterances w/ speakers | official docs |

API key creation requires a Granola Business plan or higher
(docs.granola.ai: "Any workspace member on a Business plan can create API
keys"). A free/individual-plan user cannot obtain an API key and will get a
401 at the connect step. All fields are optional in the contract; when
transcripts are absent the row carries no `transcript_ref`.

## Access & auth

- REST: `GET /v1/notes`, `GET /v1/notes/{id}`; Bearer token (user-level API
  key — returns only notes owned by/shared with the user). Base URL per
  docs.granola.ai.
- Rate limits: 25 req/5s burst, 300/min sustained — trivially fine for a
  periodic personal pull.
- No TCC, no local files. Standalone-clean (plain HTTPS). The official/
  community MCP servers exist but require a running client — not a path for
  the compiled-in collector (could be a labeled opt-in M6 later).

## Vault mapping

- **Raw layer:** `meetings/granola/raw/YYYY-MM.jsonl` — the API note objects,
  full fidelity.
- **Contract layer:** `meetings/granola/YYYY-MM.jsonl` per the (pending)
  meetings contract — expected shape: one row per meeting (`ts`, `source`,
  `guid` = note id, `title`, `duration_secs`, `attendees[]`, `summary`),
  transcripts as sidecar documents (they're artifacts, not events), overflow
  in `extra`. Does **not** fit correspondence (granularity mismatch: one
  meeting ≠ one message; readers of the message timeline don't want
  utterance floods; the interesting fields would all land in `extra`).
- **Dedupe:** note id as `guid`; cursor in `.trove/granola-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/granola.rs`: `DEF` (Periodic, hourly-ish),
   `CONNECTION` (TokenPaste: label/help/placeholder per the SimpleFIN
   affordance rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from docs.granola.ai example responses (summary-only AND
   transcript-bearing variants); parser + store + cursor tests, unique temp
   dirs.
4. Privacy gate: ships opt-in (meeting transcripts/summaries are
   conversation content) — explicit acknowledgement on enable.
5. Vault writes via `store` helpers once the meetings contract is ratified;
   until then this provider is **parked behind Needs-David (contract)**.

## API notes (corrected from brief)

- **Plan requirement:** API key creation requires a Granola Business plan or
  higher. The official docs state: "Any workspace member on a Business plan
  can create API keys." A free/individual-plan user receives a 401 at the
  connect step — they cannot obtain an API key at all. Earlier versions of
  this brief incorrectly claimed the API was available on all plans; that
  claim has been corrected.
- **List endpoint** (`GET /v1/notes`) returns only `NoteSummary` objects (id,
  title, owner, created_at, updated_at) — no attendees, no calendar event,
  no transcript. A **second call** to `GET /v1/notes/{id}?include=transcript`
  is required per note to get the full detail.
- **`web_url` vs `meeting_url`:** the API's `web_url` field is the Granola
  note permalink (`notes.granola.ai/d/<uuid>`), not a join/conference URL.
  The meetings contract's `meeting_url` field is defined as the join URL
  (e.g. a Zoom join link). Since Granola exposes no platform join URL, we
  store `web_url` in `extra.web_url` and leave `meeting_url` empty.
- **Pagination:** `hasMore` boolean + `cursor` string (not `next_cursor` as
  in Fathom). Field confirmed from the OpenAPI spec.
- **Attendee shape:** `attendees[].email` + optional `.name`; plus
  `calendar_event.invitees[].email` as fallback.
- **Transcript shape:** `transcript[].speaker.source` ("microphone"|"speaker")
  + `.diarization_label` + `.text` + `.start_time` + `.end_time`.
- **cursor strategy:** `updated_after` query param (not a traversal cursor;
  the pagination cursor is separate). Watermark is `last_updated_at`.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata + summaries | ✅ built | paste a real API key (Business plan required) in the connect card; Sync now; confirm rows in `meetings/granola/` + hub last-data |
| Transcripts | ✅ built | transcript sidecars written to `meetings/granola/raw/transcripts/<id>.jsonl`; `transcript_ref` set on contract row; requires Business plan account |
| Note permalink | ✅ built | confirm `extra.web_url` on contract row contains `granola.ai` URL; `meeting_url` field stays empty (no join URL exposed by API) |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Granola
Meeting Notes (L498–L505). Feasibility 🟢 high. Personal API key is
user-level only; the admin-level Enterprise API is separate and out of
scope. Fathom / Zoom / Fireflies follow the same meetings contract —
sequence one of them right after Granola to exercise the contract with a
second source.
