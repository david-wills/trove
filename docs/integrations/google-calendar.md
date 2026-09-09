# Google Calendar

- **id:** `google-calendar`
- **domains:** `calendar/` (contract: **✅ ratified** — shared with EventKit
  / Apple Calendar and CalDAV)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll events; incremental via `syncToken` /
  `updatedMin` watermark)
- **connection:** `google` — OAuth, the shared Google connection (one login,
  six defs: Calendar, Contacts, Gmail, Tasks, YouTube, Takeout-class pulls).
  Calendar read scope is part of the full-scope consent bundle.
- **evidence:** official-docs — Google Calendar API v3
  (`googleapis.com/calendar/v3`), stable since 2011, with example responses
- **effort / priority:** S / P1
- **needs:** none

## What it is

Google's calendar service — one of the two most common personal calendars
(with Apple Calendar). Holds events, attendees, recurrence, locations, and
reminders across all of a user's Google calendars. Pairs naturally with the
already-shipped Apple Calendar def: both write the ratified `calendar`
contract, so the read-time calendar view merges them.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Events (all calendars) | all accounts | title, start/end, location, attendees, recurrence, status | official v3 docs |
| Calendar list | all accounts | calendar ids, names, colors, access role | official v3 docs |

All optional in the contract (omit-if-empty). No tier gating.

## Access & auth

- REST: `GET /calendar/v3/users/me/calendarList`, then
  `GET /calendar/v3/calendars/{id}/events`. OAuth 2.0 scope
  `…/auth/calendar.readonly`. Pagination via `nextPageToken`; incremental
  sync via `syncToken`.
- Rate limits: 1M queries/day free tier — far beyond a personal periodic
  pull.
- No TCC, no local files; plain HTTPS, standalone-clean. CalDAV at
  `…/caldav/v2/calendars` is an alternate path (the generic CalDAV def covers
  it) but the v3 API is the primary route here.
- The `writerWithoutPrivateAccess` access-level rollout (June 29 2026) does
  not affect read-only pulls.

## Vault mapping

- **Raw layer:** `calendar/google-calendar/raw/YYYY-MM.jsonl` — the v3 event
  objects, full fidelity.
- **Contract layer:** `calendar/…` per the **ratified** calendar contract —
  one row per event (`ts` = start, `source`, `guid` = event id + calendar id,
  `title`, `end`, `location`, `attendees[]`, `status`, `recurrence`),
  overflow in `extra`.
- **Dedupe:** `{calendar_id}:{event_id}` as `guid`; `syncToken` cursor in
  `.trove/google-calendar-sync.json`, rebuildable by re-scanning.

## Build plan

Shipped **pre-pipeline** as the `google-calendar` def on the shared `google`
connection — module, fixtures, and tests already landed before this catalog
pass. No new build work; this brief is the catalog record. Remaining steps are
validation only (below) and David's promotion 🧪 → ✅.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Events | 🧪 built (pre-pipeline) | shipped before the pipeline; David signs in with a real Google account, Sync now, confirms events in `calendar/` + hub last-data, then promotes 🧪 → ✅ |
| Multi-calendar | 🧪 built (pre-pipeline) | confirm secondary/shared calendars all appear and merge with Apple Calendar rows at read time |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity" §Google
Calendar (L2506–L2512). Feasibility 🟢 high. Stable v3 API since 2011; CalDAV
alternative exists. The `writerWithoutPrivateAccess` change (June 29 2026) is
read-safe. Shares the calendar contract with EventKit/Apple Calendar and the
generic CalDAV connector.
