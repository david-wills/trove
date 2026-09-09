# Fathom

- **id:** `fathom`
- **domains:** `meetings/` (contract: **Phase 3 pending** — drafted from
  Granola + Fathom + Zoom + Fireflies together)
- **status:** 🧪 built (fixture-tested; first-in-domain binding of `meetings/`; embedded-transcript API poll; needs a real API key to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (poll the meetings list; watermark cursor — no
  webhooks: a push endpoint would violate local-first)
- **connection:** `fathom` — TokenPaste (`X-Api-Key` from Fathom settings).
  Not shared with other defs.
- **evidence:** official-docs — developers.fathom.ai (REST API at
  api.fathom.ai/external/v1, TypeScript/Python SDKs, MCP docs); 60 calls/min
  rate limit documented
- **effort / priority:** S / P1
- **needs:** privacy (meeting content ≈ message bodies — opt-in with explicit
  acknowledgement) · Needs-login (validation only — build proceeds from
  documented shapes) · meetings contract not yet ratified (Needs-David)

## What it is

AI video notetaker that records calls (Zoom/Meet/Teams) and produces
transcripts with speaker labels and timestamps, plus highlights and
summaries. Same shape and value as Granola; the second source that
exercises the meetings contract.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Meeting list + metadata | no plan gating documented | title, time, participants, highlights (`include_highlights=true`) | official docs |
| Full transcript | no plan gating documented | speaker-labeled, timestamped utterances | official docs |

All optional in the contract; transcripts arrive asynchronously after a
call ends, so a freshly-ended meeting may appear metadata-only on one poll
and gain its transcript on the next — the upsert-by-guid path must handle
that without duplicate rows.

## Access & auth

- REST at `https://api.fathom.ai/external/v1`; auth via `X-Api-Key` header
  (user-level key from Fathom settings — only reaches meetings recorded by
  that user or shared to their team; admin keys do not reach other users'
  unshared meetings).
- Key endpoints: `GET /meetings` (list, `include_highlights=true`),
  `GET /meetings/{id}/transcript`.
- Rate limit 60 calls/min — fine for a periodic personal pull.
- Webhooks exist but require a public endpoint — poll instead. MCP
  implementations exist (official docs + community) but need a running
  client — not a path for the compiled-in collector.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `meetings/fathom/raw/YYYY-MM.jsonl` — API meeting objects +
  transcript payloads, full fidelity.
- **Contract layer:** `meetings/fathom/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per meeting (`ts`, `source`, `guid` = meeting
  id, `title`, `duration_secs`, `attendees[]`, `summary`/highlights),
  transcript as sidecar document, overflow in `extra`.
- **Dedupe:** meeting id as `guid`; transcript-arrives-later updates the
  same row. Cursor in `.trove/fathom-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/fathom.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste with help copy pointing at Fathom settings), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from developers.fathom.ai example responses — include a
   metadata-only (transcript-pending) variant and a transcript-bearing
   variant; test the late-transcript upsert.
4. Privacy gate: ships opt-in (meeting transcripts are conversation
   content) — explicit acknowledgement on enable.
5. Parked behind the meetings contract (Needs-David) like Granola; build in
   the same wave so the contract is drafted against both shapes.

## Build status — 🧪 2026-06-15

Shipped (`fathom.rs`, INDEX #22 — also the **first-in-domain binding** of the
`meetings/` contract). `Behavior::Periodic` (15 min), `CONNECTION` = TokenPaste
(`X-Api-Key` from Fathom settings, stored 0600, never logged), **🔒 default-off**
(meeting transcripts ≈ conversation content). Built **Opus** (first-in-domain
bind, per the model policy); a Sonnet evidence spike fed it.

- **Binding (first in `meetings/`):** new `meetings.rs` `Meeting` struct matching
  `meetings.meeting.schema.json` (required `ts`/`source`/`guid`; title/started/
  ended/duration_secs/platform/attendees/attendee_names/host/summary/meeting_url/
  recording_url/folder/transcript_ref/extra omit-empty); `meetings` `DOMAINS`
  (EventStream, month of `ts`); promoted the draft fixture to the ratified triad
  (5/5). **Binds the shape 6 future meetings sources reuse** (granola/otter/
  fireflies/read-ai/tldv/krisp). `attendee_names` emitted only when fully aligned
  with `attendees` (else → `extra`).
- **Pull:** `GET /external/v1/meetings?include_summary=true&include_highlights=true&
  include_transcript=true` → `{items, next_cursor}`. `guid` = `recording_id`
  (int→string); `ts` = `recording_start_time` (ISO-8601 UTC → local); `duration_secs`
  computed `end − start` (no duration field exists); `summary` from
  `default_summary.markdown_formatted`; `highlights` → `extra`; `attendees` from
  `calendar_invitees[].email` (lowercased). → `meetings/fathom/YYYY-MM.jsonl`
  (contract) + raw + transcript sidecars.
- **Transcripts are EMBEDDED** (API doc: *"include_transcript … Unavailable for
  OAuth connected apps (use /recordings instead)"* — so for `X-Api-Key` auth the
  transcript is the meeting object's nullable `transcript` array; **there is NO
  separate `/recordings/{id}/transcript` for this flow**). Utterances
  (`{speaker, text, timestamp:"HH:MM:SS"}`) → the sidecar `meetings/fathom/raw/
  transcripts/<recording_id>.jsonl`; `transcript_ref` points at it (omitted until
  present).
- **Async transcripts handled by design, no extra machinery:** drain the list
  (`next_cursor`→null) **every poll** and **upsert by `guid`** — a meeting with a
  null transcript writes a metadata-only row; on whatever later poll its embedded
  transcript appears, the same `guid` row gains `transcript_ref` (one row, never a
  duplicate). The watermark (`last_meeting_ts`) is a **write-filter, never a
  traversal cutoff** (drains all pages so nothing strands; a 30-day recheck window
  catches a late transcript on an already-stored meeting).

Evidence: the API shapes + the **embedded-transcript** mechanism confirmed against
developers.fathom.ai's API reference. Adversarial-verify (Opus): **2 BLOCKING + 1
minor, all fixed** — (B) the build first fetched transcripts from a non-existent
`/recordings/{id}/transcript` endpoint (a wrong API fact from the evidence spike) →
switched to the embedded `include_transcript=true` field; (B) pagination stopped
early on the watermark, which would strand meetings on later pages since Fathom's
list order is undocumented → now drains every page with a write-filter watermark;
(m) an unbounded transcript-recheck set → bounded to a 30-day window (cursor stays
a single timestamp). The fix collapsed the async machinery (no separate endpoint,
no pending map). The binding triad, guid stability, attendee alignment, and
secret-safety were independently confirmed.

Gate (my run, serial): trove-core 670/0, `cargo check` clean, spec_validation 5/5
(meetings ratified), `schedule_doc` regenerated (fathom Periodic), `bindings.ts` up
to date. **Deferred:** none (action-items/keywords ride in `extra`).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Meeting list + metadata | 🧪 (Needs-login) | Fathom → Settings → API → create a key; paste it in the connect card; Sync now; confirm rows in `meetings/fathom/` + hub last-data |
| Transcripts (embedded) | 🧪 (Needs-login) | record a Fathom call, wait for processing, re-sync; confirm the existing `guid` row gains `transcript_ref` + the sidecar `meetings/fathom/raw/transcripts/<id>.jsonl`, with NO duplicate row |
| Pagination | 🧪 (Needs-login) | an account with many meetings: confirm all months populate (full drain, not just the first page) |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Fathom
Video Notetaker (L506–L513). Feasibility 🟢 high. The deferred-tools list
already includes `mcp__fathom__authenticate` — irrelevant to the compiled-in
collector but confirms the auth flow is conventional. Async transcript
processing is the one behavioral gotcha; otherwise the cleanest of the four
meetings sources alongside Granola.
