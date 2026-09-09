# CalDAV (any server)

- **id:** `caldav`
- **domains:** `calendar/` (contract: **calendar** — ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (REPORT the configured calendars on a schedule;
  ETag/CTag watermark to fetch only changed objects)
- **connection:** `caldav` — TokenPaste (server base URL + username +
  password / app-specific password). Not shared; this is the generic
  protocol connector, distinct from the Google OAuth and EventKit paths.
- **evidence:** official-docs — open standard RFC 4791 (CalDAV); iCalendar
  (RFC 5545) for the payload. Rust crates `caldav-client` / `mini-dav` +
  `ical` parse both. No vendor docs needed — protocol is the spec.
- **effort / priority:** M / P1
- **needs:** none (highest-breadth calendar connector; carries no privacy
  flag — calendar events are not message bodies)

## What it is

The protocol under iCloud Calendar, Fastmail, Nextcloud, Proton Calendar
(via Bridge, 2024+), Radicale/Baikal self-hosting, and Google Calendar's
alternate path. One generic connector that any RFC-4791-compliant server
answers — the catch-all that covers every calendar Trove doesn't have a
dedicated integration for. Self-hosters and privacy-minded users (Proton,
Fastmail) get their calendar with no bespoke per-provider work.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Calendar discovery | all servers | calendar-home-set, display names, CTags | RFC 4791 PROPFIND |
| Event objects | all servers | VEVENTs — start/end, summary, location, attendees, status, RRULE, UID | RFC 5545 |
| Change detection | all servers | per-object ETag, per-collection CTag/sync-token | RFC 4791 |

All optional in the contract; a minimal server that returns only summary +
start still maps cleanly.

## Access & auth

- HTTP `PROPFIND` (discover calendars) then `REPORT` (`calendar-query` /
  `calendar-multiget`) against the user's server base URL; payloads are
  `.ics`. Auth is HTTP Basic over TLS.
- Known servers: `caldav.icloud.com`, `calendar.google.com/caldav/v2`,
  Fastmail, Nextcloud, self-hosted Radicale/Baikal. iCloud needs an
  app-specific password (2FA accounts); Proton needs Bridge running.
- Rate limits: server-dependent and generous for personal polling; honor
  `sync-token` / CTag so a poll fetches only drift, not the full set.
- Standalone-clean: plain HTTPS, libraries compiled in. No TCC.

## Vault mapping

- **Raw layer:** `calendar/caldav/raw/YYYY-MM.jsonl` — the parsed VEVENT
  objects per account, full fidelity (RRULE, VALARM, custom X- props).
- **Contract layer:** the ratified **calendar** contract —
  `calendar/events/YYYY-MM.jsonl` (occurrence snapshot, recurrences
  expanded, partitioned by month of `start`) + `calendar/changes/YYYY-MM.jsonl`
  (diff stream the sync observes). Field map: VEVENT `SUMMARY`→`title`,
  `DTSTART`/`DTEND`→`start`/`end` (RFC3339 local; all-day → local midnight→
  23:59:59), `LOCATION`, `ATTENDEE`→`attendees[]`, `STATUS`→`status`
  (`confirmed`/`tentative`/`canceled`), `calendar` = collection display name,
  `account` = the configured server/username.
- **Dedupe:** `guid` from the VEVENT `UID` (+ `RECURRENCE-ID` for a single
  moved instance). First sync writes snapshot, no `added` lines (state, not
  events). Sync-token/CTag cursor in `.trove/`, rebuildable from output.

## Build plan

1. Module `crates/trove-core/src/caldav.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: server URL + username + password fields, label/help/
   placeholder per the SimpleFIN affordance rule — name iCloud/Fastmail/
   Nextcloud as examples).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. PROPFIND discovery → REPORT fetch via `caldav-client`/`mini-dav`; parse
   `.ics` with `ical`; expand RRULE for the occurrence snapshot.
4. Fixtures: hand-authored `.ics` covering recurring + all-day + canceled +
   timezone-bearing events; parser/store/diff tests, unique temp dirs.
5. Reuse the EventKit collector's contract-write helpers (same calendar
   contract) so occurrence/change writing isn't reimplemented.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Event sync | 🔲 Needs-login | configure an iCloud app-specific password (or Fastmail/Nextcloud) in the connect card; Sync now; confirm occurrences in `calendar/events/` + hub last-data |
| Change stream | 🔲 Needs-login | reschedule an event server-side, re-sync, confirm a `changed` line in `calendar/changes/` |
| Self-hosted | 🔲 Needs-login | point at a Radicale/Baikal URL; confirm same shape (any real user's server validates this slice) |

## Build notes (2026-06-16)

Built as follower of the `calendar` pioneer. Implementation:

- **HTTP layer:** pure ureq (already in deps) + quick-xml (already in deps). No new HTTP crate.
- **ICS parsing:** `ical = "0.11.0"` crate added (pure Rust, no C deps).
- **RRULE expansion:** hand-rolled within the sync window for FREQ=DAILY/WEEKLY/MONTHLY/YEARLY. BYDAY respected for WEEKLY. Complex rules (BYMONTHDAY/BYSETPOS) emit the base occurrence with `recurring=true`; full RRULE in raw layer.
- **Row ownership:** `caldav:` prefix, scoped via `calendar_snapshot_scoped` — no interference with EventKit or Google Calendar rows.
- **Credentials:** `TokenPaste` composite (URL\nusername\npassword). Password in 0600 secret store only; sync cursor (CTag watermark) is credential-free.
- **25 unit tests green.**

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§CalDAV (Generic) (L2626–L2632). Feasibility 🟢 high. The single
highest-breadth calendar connector: one protocol, every compliant server.
iCloud/Proton require app-specific-password / Bridge setup — surface that in
connect-card help. Distinct from the dedicated EventKit and Google Calendar
paths, which read those same calendars natively; CalDAV is the fallback for
everything else.
