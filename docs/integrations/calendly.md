# Calendly

- **id:** `calendly`
- **domains:** `calendar/` (contract: **calendar** — ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll `/scheduled_events`; rolling [now-365d, now+365d] window; upsert on status change)
- **connection:** `calendly` — TokenPaste (personal access token from
  Calendly Settings > Integrations > API & Webhooks; no app registration).
  OAuth 2.1 is an alternative but PAT is the standalone-clean path. Not
  shared with other defs.
- **evidence:** official-docs — api.calendly.com v2 (`/scheduled_events`,
  `/scheduled_events/{uuid}/invitees`, documented 100 req/min limit, status
  field on each event)
- **effort / priority:** S / P2
- **needs:** none

## What it is

Scheduling tool: people book time on the user's Calendly links and the
booked meetings accumulate as a scheduling history. Used heavily by anyone
who takes external meetings (sales, recruiting, consulting, freelancers).
The value here is the *booked-meeting record* — who booked, when, for what
event type, and whether they cancelled — which the user's own calendar may
not fully capture.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Scheduled events | all plans (PAT) | uri, name, start/end time, status (active/cancelled), event-type, location | official docs |
| Invitees | all plans | per-event attendee name, email, timezone, questions/answers | official docs |

All optional in the contract; a free-tier user's events simply carry fewer
fields. No tier-specific code paths.

## Access & auth

- REST: `GET https://api.calendly.com/scheduled_events` (lists all events
  including cancelled), `GET /scheduled_events/{uuid}/invitees` for attendee
  detail. Bearer PAT (user-level — returns only the user's own bookings).
- Rate limit: 100 req/min — trivially fine for a periodic personal pull.
- No TCC, no local files. Standalone-clean (plain HTTPS). Webhooks exist
  for live capture but polling fully suffices for a personal record.

## Vault mapping

- **Raw layer:** `calendar/calendly/raw/YYYY-MM.jsonl` — the API event +
  invitee objects, full fidelity.
- **Contract layer:** `calendar/calendly/YYYY-MM.jsonl` per the ratified
  **calendar** contract — one row per booked event (`ts` = start time,
  `source`, `guid` = event uri/uuid, `title` = event-type name, `end`,
  `attendees[]` from invitees, `status`); cancelled events kept with their
  status, never dropped. Overflow (event-type uri, questions/answers,
  location detail) in `extra`.
- **Dedupe:** event uri/uuid as `guid`; cursor in
  `.trove/calendly-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/calendly.rs`: `DEF` (Periodic, hourly-ish),
   `CONNECTION` (TokenPaste: label/help/placeholder per the SimpleFIN
   affordance rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`; `connection:
   Some("calendly")`.
3. Fixtures from api.calendly.com example responses (active + cancelled
   events; an event with invitees); parser + store + cursor tests, unique
   temp dirs.
4. Map onto the ratified calendar contract via existing `store` helpers —
   no contract work needed.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Scheduled events | 🧪 built | paste a real PAT in the connect card; Sync now; confirm rows in `calendar/calendly/` + hub last-data |
| Invitees | 🧪 built | confirm an event row carries `attendees[]` populated from the invitees call |

## Implementation notes (Phase 4)

- Module: `crates/trove-core/src/calendly.rs` — Periodic/hourly, TokenPaste PAT, new
  `CONNECTION` (id = "calendly").
- Contract: `CalendarOccurrence` rows in `calendar/calendly/YYYY-MM.jsonl` (per-source
  path, separate from the shared `calendar/events/` EventKit/Google store). Row `id`
  prefixed `cly:<uuid>`. Cancelled events kept with `status = "canceled"`.
- Raw: full-fidelity event + invitees JSON in `calendar/calendly/raw/YYYY-MM.jsonl`.
  The raw envelope preserves all fields verbatim (event-type URI, location detail,
  questions/answers, cancellation details).
- Cursor: `.trove/calendly-sync.json` (`initialized` flag + `updated` timestamp).
  Query window is ALWAYS rolling `[now - 365d, now + 365d]` — no start-time watermark
  is stored. Advancing min_start_time via a stored watermark would cause "lookahead
  poison": a future-dated booking would raise the floor, silently stranding all
  near-term bookings beneath it. The rolling window also picks up status changes
  (active→canceled) for events already stored; those rows are upserted in place.
- First pull: `/users/me` → user URI; then `/scheduled_events?user=…&status=active,canceled`
  paged via `pagination.next_page_token`; then per-event `/invitees`.
- 14 unit tests, all green; `cargo check` clean.

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Calendly (L2554–L2560). Feasibility 🟢 high. PAT needs no OAuth app
registration. Webhooks available but polling chosen for the standalone
collector. Routes whole to `calendar/` — Calendly produces calendar events,
not tasks.
