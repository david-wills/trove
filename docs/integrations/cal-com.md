# Cal.com

- **id:** `cal-com`
- **domains:** `calendar/` (contract: **calendar** — ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll `/v2/bookings`; updated-since/watermark cursor)
- **connection:** `cal-com` — TokenPaste (API key from Cal.com Settings →
  Developer → API keys; instant, no OAuth). Self-hosted instances use the
  same connection with a custom base URL. Not shared with other defs.
- **evidence:** official-docs — api.cal.com v2 (`GET /v2/bookings`,
  documented response shape); open-source project.
- **effort / priority:** S / P2
- **needs:** none (scheduling/booking metadata, not message bodies)

## What it is

Open-source Calendly alternative for booking pages and scheduled meetings.
Hosted Cal.com (free tier accessible) and self-hosted instances both expose
the same REST v2 API. The interesting data is the **booking history** — who
booked time with you, for what event type, when, and whether they showed or
cancelled — a record of scheduled meetings that doesn't otherwise land in a
plain calendar feed with the same structure (event type, attendee, status).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Bookings | free tier OK | start/end, title, event type, attendee(s), status (incl. cancelled) | official v2 docs |
| Event types | free tier OK | name, duration, slug | official v2 docs |

All optional in the contract; cancelled bookings are included and map to
`status: "canceled"`.

## Access & auth

- REST v2: `GET https://api.cal.com/v2/bookings`; Bearer API key from
  Settings → Developer → API keys. Self-hosted: same endpoints at the
  instance's custom base URL (configurable on the connection).
- Rate limits: documentation sparse but the API is free-tier accessible;
  a periodic personal poll is well within any reasonable cap.
- Standalone-clean: plain HTTPS, no TCC, no local files.

## Vault mapping

- **Raw layer:** `calendar/cal-com/raw/YYYY-MM.jsonl` — the API booking
  objects, full fidelity (event-type metadata, attendee details, custom
  responses).
- **Contract layer:** the ratified **calendar** contract —
  `calendar/events/YYYY-MM.jsonl` (occurrence snapshot, partitioned by month
  of `start`) + `calendar/changes/YYYY-MM.jsonl`. Field map: booking
  `title`/event-type name→`title`, `start`/`end`→`start`/`end` (RFC3339
  local), attendee email/name→`attendees[]`, booking `status`→`status`
  (`accepted`→`confirmed`, `cancelled`→`canceled`), `calendar` = "Cal.com"
  (or event-type), `account` = the connected Cal.com user. Event-type slug /
  booking responses overflow to `extra`.
- **Dedupe:** `guid` = booking uid/id. Updated-since cursor in `.trove/`,
  rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/cal_com.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: API key field + optional base-URL field for self-hosted,
   label/help/placeholder per the affordance rule), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented v2 booking response (confirmed + cancelled
   variants); parser + store + cursor tests, unique temp dirs.
4. Reuse the calendar contract-write helpers (shared with EventKit/CalDAV).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Bookings | ✅ built | paste a real API key in the connect card; Sync now; confirm rows in `calendar/events/` + hub last-data |
| Cancelled bookings | ✅ built | cancel a test booking, re-sync, confirm `status: "canceled"` + a `changed`/`removed` line in `calendar/changes/` |
| Self-hosted | ✅ built | point the base URL at a self-hosted instance; `key\|https://instance.com` format (stored in token_type) |

## Build notes (2026-06-16)

- Module: `crates/trove-core/src/cal_com.rs`; replaces the Phase-2 `NotWired` stub.
- TokenPaste connection (`cal-com` service id); composite credential `key[|base_url]` for self-hosted support; base URL stored in `token_type` slot.
- API version header `cal-api-version: 2026-05-01` confirmed from official v2 docs.
- Ownership prefix `calcom:` for `calendar_snapshot_scoped` — does not overlap Google (`gcal:`), Outlook (`mcal:`), or CalDAV (`caldav:`) rows.
- Incremental: `afterUpdatedAt` watermark in `.trove/cal-com-sync.json`; full fetch on first run.
- 10 unit tests pass (cargo test -p trove-core cal_com::); cargo check green.

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Cal.com (L2634–L2640). Feasibility 🟢 high. API key is instant; bookings
include cancelled events. The Cal.diy self-hosted fork went closed-source in
2025 but hosted Cal.com stays open and the v2 API is stable. Self-hosted
users could alternatively query PostgreSQL directly (M3/M6) — out of scope
for the compiled-in collector, the REST API covers both.
