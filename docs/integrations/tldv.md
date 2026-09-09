# tl;dv

- **id:** `tldv`
- **domains:** `meetings` (contract: **not yet ratified** — Phase 3 drafts it
  from Granola + Fathom + Zoom + Fireflies together; tl;dv conforms once
  ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll; the documented real-time path is
  webhook-only, which Trove doesn't accept — see Access)
- **connection:** `tldv` — TokenPaste (API key; **Business plan only** — the
  free tier has no API access)
- **evidence:** official-docs — API & webhooks article
  (intercom.help/tldv/…/11583137-api-and-webhooks); polling fallback
  **unconfirmed** in those docs
- **effort / priority:** S / P2
- **needs:** Privacy-sensitive (opt-in) · Needs-login (Business-plan account
  to confirm polling + validate) · Needs-David (contract: meetings)

## What it is

Meeting recorder/notetaker covering Zoom, Google Meet, and Teams with 40+
language support; large integration surface via Zapier/n8n. Programmatic
access (REST API + webhooks) is gated to the Business plan, so this
integration serves paying users; free-tier users have only the dashboard
export (TXT/Markdown/CSV) as a manual backup path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Meetings + transcripts via API | Business plan | transcript, meeting metadata | official API article |
| Webhook on transcript-ready | Business plan | same payload, push | official API article (not used — local-first) |
| Dashboard export | all paid tiers (per docs) | TXT/Markdown/CSV transcript | official article |

All optional in the contract; non-Business users simply can't connect (the
card says so plainly, per the disabled-controls-affordance rule).

## Access & auth

- REST API + webhooks, Business plan: API-key auth (no OAuth mentioned).
  Webhook fires when a transcript is ready — needs a public endpoint, so
  Trove polls instead; **whether the REST API supports list-style polling is
  unconfirmed** and is the first thing the build verifies against live docs
  (Phase 4 rule).
- Fallback: dashboard TXT/MD/CSV export through the generic import box if
  polling proves unavailable (behavior would shift to Import).
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `meetings/tldv/raw/YYYY-MM.jsonl` — API payloads full
  fidelity (or verbatim export files if the import fallback ships).
- **Contract layer:** `meetings/tldv/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per meeting (`ts`, `source`, `guid` = meeting
  id, `title`, `attendees[]`); transcript as sidecar document; overflow in
  `extra`.
- **Dedupe:** API meeting id as `guid` (content hash for imported exports);
  cursor in `.trove/tldv-sync.json`, rebuildable.

## Build plan

1. Verify against live docs first (Phase 4 loop step): does the Business
   API expose a meetings-list endpoint suitable for polling? If yes →
   Periodic as specced; if webhook-only → downgrade this brief to Import
   (dashboard exports) and note it here.
2. Module `crates/trove-core/src/tldv.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste; setup copy must state the Business-plan requirement so the
   gated connect card carries its affordance hint), `pull` hook.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from the API article's examples (thin — supplement from a real
   Business account at validation, not build, time); parser + store + cursor
   tests.
5. Privacy: transcripts are message-body-class — default-off, explicit
   opt-in.
6. Parked behind Needs-David (meetings contract).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| API pull | — | Business-plan account (David doesn't have one — any real user's run can validate); paste key; Sync now; rows in `meetings/tldv/` |
| Export import fallback | — | export TXT/MD/CSV from a dashboard; import; confirm dedupe against API-pulled rows |

## Implementation notes (built 2026-06-16)

- REST API at `https://pasta.tldv.io`; auth via `x-api-key: <key>` header.
  Verified from the official API docs at `doc.tldv.io` and
  `intercom.help/tldv/en/articles/11583137-api-and-webhooks`.
- **Polling IS supported**: `GET /v1alpha1/meetings?pageSize=N&page=P` returns
  paginated meetings with `{results[], page, pages, total, pageSize}`.
  This resolves the "unconfirmed polling" question from the brief — the API
  has a proper list endpoint suitable for periodic polling.
- Meeting object fields: `id`, `name`, `happenedAt` (ISO8601), `url`
  (recording URL), `duration` (seconds as float), `organizer` (name+email),
  `invitees[]` (name+email), `extraProperties.conferenceId`.
- Separate `GET /v1alpha1/meetings/{id}/transcript` → `{data: [{speaker, text, startTime, endTime}]}`.
- Separate `GET /v1alpha1/meetings/{id}/notes` → `{markdownContent, structuredNotes[], topics[]}`.
  `markdownContent` → `Meeting.summary`; topics + structuredNotes → `extra`.
- Plan gating: **Pro or Business** (not just Business as the brief assumed —
  the API docs say "Pro or Business plans only").
- Watermark: `last_happened_at` in `.trove/tldv-sync.json`. Full drain on
  every poll; write-filter skips already-seen meetings. Cursor advances only
  after a complete successful pass.
- 13 unit tests, all green. `cargo check` clean.
- CONNECTION registered in `INTEGRATIONS` and `CONNECTIONS` (one new
  `&crate::tldv::CONNECTION,` line added to `integrations.rs`).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| API pull (list) | ✅ built | Pro/Business account: paste key; Sync now; rows in `meetings/tldv/` |
| Transcript sidecar | ✅ built | check `meetings/tldv/raw/transcripts/<id>.jsonl` for speaker-labeled segments |
| Notes/summary | ✅ built | `Meeting.summary` populated from markdownContent; topics in `extra` |
| Watermark / incremental | ✅ built | second sync shows 0 new meetings; `.trove/tldv-sync.json` has last_happened_at |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §tl;dv
Meeting Recorder (L594–L601). Feasibility was 🟡 medium due to unconfirmed
polling story — **resolved**: the REST API has a full list endpoint. Plan
gating is Pro or Business (confirmed; brief said Business-only which was
conservative). Build is now complete.
