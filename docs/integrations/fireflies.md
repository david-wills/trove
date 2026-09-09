# Fireflies.ai

- **id:** `fireflies`
- **domains:** `meetings/` (contract: `meetings.Meeting` — reused from the
  ratified meetings contract, same as Fathom/Granola)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (GraphQL poll, date-filtered; watermark cursor —
  webhooks exist but a push endpoint violates local-first, so poll)
- **connection:** `fireflies` — TokenPaste (API key from Fireflies settings;
  available on **all plans including free**). Not shared with other defs.
- **evidence:** official-docs — api.fireflies.ai/graphql GraphQL API,
  fully documented at docs.fireflies.ai; official MCP configuration
  documented; open-source MCP exists (Props-Labs/fireflies-mcp)
- **effort / priority:** S / P1
- **needs:** privacy (meeting content ≈ message bodies — opt-in with explicit
  acknowledgement)

## What it is

AI meeting assistant that joins meetings as a bot participant, records, and
transcribes. Returns the richest per-meeting data of the four P1 meetings
sources: sentence-level transcripts with speaker attribution and per-sentence
AI tags (action item / question / sentiment), structured summaries
(action_items, keywords, outline), and speaker analytics. Notably the only
one with full API access on the free plan.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transcript list + metadata | all plans incl. free | title, date, participants | official docs |
| Sentence-level transcript | all plans (no gating documented) | sentences w/ speaker, start/end time, text, AI tags | official docs |
| Structured summary | all plans | action_items, keywords, outline | official docs |
| Analytics | all plans | speaker stats | official docs |

All optional in the contract; GraphQL lets the pull request exactly the
fields it needs, so over-fetching is a non-issue.

## Access & auth

- GraphQL at `https://api.fireflies.ai/graphql`; header
  `Authorization: Bearer <api_key>`.
- Queries: transcripts list (filter by date) → transcript by ID (sentences,
  summaries, analytics).
- API returns data only for meetings the user's Fireflies account attended
  — naturally scoped, no admin-reach concerns.
- No TCC, no local files. Standalone-clean (plain HTTPS). MCP servers exist
  but require a running client — not a path for the compiled-in collector.

## Vault mapping

- **Raw layer:** `meetings/fireflies/raw/YYYY-MM.jsonl` — full GraphQL
  transcript objects (sentences, tags, analytics), full fidelity.
- **Contract layer:** `meetings/fireflies/YYYY-MM.jsonl` per the ratified
  meetings contract (`meetings.Meeting`, same binding as Fathom/Granola) —
  one row per meeting (`ts`, `source`, `guid` = transcript id, `title`,
  `duration_secs`, `attendees[]`, `summary` from the outline/action-items
  block), sentence-level transcript as sidecar document, per-sentence AI
  tags stay in the raw layer / `extra`.
- **Dedupe:** transcript id as `guid`; cursor in
  `.trove/fireflies-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/fireflies.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste; help copy points at Fireflies settings → API
   key, noting it works on the free plan), `pull` hook. The GraphQL call is
   a plain HTTPS POST with a query string — no GraphQL client dependency
   needed.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from docs.fireflies.ai example responses (list + by-id with
   sentences/summary/analytics); parser + store + cursor tests, unique temp
   dirs.
4. Privacy gate: ships opt-in (sentence-level transcripts of conversations,
   bot-recorded) — explicit acknowledgement on enable.
5. Reuses the ratified `meetings.Meeting` contract (same as Fathom/Granola
   pioneer); sentence-tag richness confirms the sidecar + `extra` split holds.

## Implementation notes (built 2026-06-15)

- GraphQL POST to `api.fireflies.ai/graphql`; `Authorization: Bearer <key>`.
- Two-pass per transcript: `transcripts(limit:50, skip:N, fromDate:$watermark)`
  list (metadata + summary) → `transcript(id: $id)` detail (sentences + full
  summary). Detail is only fetched for NEW transcripts to conserve the 50
  req/day free-plan budget.
- Pagination: `skip`/`limit` (not cursor). Drain all pages per poll.
- Watermark: `last_date_ms` in `.trove/fireflies-sync.json` — the `date` field
  is epoch milliseconds (Float in GraphQL). Passed as `fromDate` ISO string on
  next poll.
- Sentence sidecar: `meetings/fireflies/raw/transcripts/<id>.jsonl`, one JSON
  utterance per line; `ai_filters` (task/question/sentiment/etc.) preserved
  verbatim. `transcript_ref` on the contract row points here.
- `meeting_attendees[].displayName/email` → `attendees`/`attendee_names`
  (aligned only when all entries have both); falls back to `participants[]`
  string array.
- Summary `overview`/`gist` → `Meeting.summary`; full `summary{}` object →
  `extra["summary"]` for full fidelity (action_items, keywords, outline, etc.).
- `duration` field: GraphQL Float in **minutes**; converted to seconds
  (`round(minutes * 60)`) for `duration_secs`. Fixtures use `49.0` (minutes)
  → `2940` seconds, matching the real wire shape.
- `recording_url` intentionally empty: `transcript_url` is a Fireflies
  dashboard page (not a recording); `audio_url`/`video_url` are signed URLs
  expiring within 24 h. `transcript_url` is preserved in `extra["transcript_url"]`
  for dashboard access.
- `platform` derived from the `meeting_link` host (`zoom.us` → `"zoom"`,
  `meet.google.com` → `"meet"`, `teams.microsoft.com` → `"teams"`, etc.).
- Rate limit: syncs every 4h (6 polls/day) to stay within free-plan budget.
  Large initial syncs may span multiple days.
- 19 unit tests, all green. cargo check clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| List + metadata + summary | ✅ built | paste a free-plan API key; Sync now; rows in `meetings/fireflies/` + hub last-data |
| Sentence-level transcript | ✅ built | have Fireflies attend one real meeting; re-sync; confirm `meetings/fireflies/raw/transcripts/<id>.jsonl` with speaker names + AI tags |
| Watermark / incremental | ✅ built | second sync shows no new meetings; `.trove/fireflies-sync.json` holds last_date_ms |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts"
§Fireflies.ai (L522–L529). Feasibility 🟢 high. Free-plan API access makes
this the easiest meetings source for any user to validate end-to-end —
a good candidate to sequence immediately after Granola to ratify the
contract against two live shapes. The bot-participant recording model means
other attendees' speech is captured; the opt-in copy should be honest about
that.
