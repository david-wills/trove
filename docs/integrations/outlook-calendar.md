# Outlook Calendar

- **id:** `outlook-calendar`
- **domains:** `calendar/` (contract: **✅ ratified** — calendar)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (Graph delta query; watermark cursor)
- **connection:** `microsoft` — OAuth (new **shared Microsoft connection**;
  baked Azure AD app, delegated `Calendars.Read`). Shared with the Microsoft
  To Do def (one login, two pulls).
- **evidence:** official-docs — graph.microsoft.com/v1.0/me/events, OAuth 2.0
  via Azure AD; OpenAPI spec published; delta query documented
- **effort / priority:** M / P1
- **needs:** none (build proceeds from documented Graph shapes; a real
  Microsoft login validates only)

## What it is

Microsoft's calendar, read via the Graph API — covers both personal
Outlook.com / Microsoft accounts and M365 / enterprise tenants. Essential for
anyone in the Microsoft ecosystem (the counterpart to the shipped Google
Calendar def). EWS (legacy Exchange) shuts down October 2026, so Graph is the
only forward path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Events | all accounts | subject, start/end, location, organizer, attendees, recurrence, isCancelled | official docs |
| Multiple calendars | all accounts | personal + shared calendars | official docs |
| Incremental sync | all accounts | `$deltaToken` cursor — only changed events | official docs |

All optional in the calendar contract; omit-if-empty fields (location,
attendees) simply absent.

## Access & auth

- REST: `GET https://graph.microsoft.com/v1.0/me/events` (and
  `/me/calendarView` for expanded recurrences); delta query
  `/me/calendars/{id}/events/delta` for incremental sync.
- Auth: OAuth 2.0 via Azure AD, delegated **`Calendars.Read`** scope. Requires
  a **baked Azure app registration (free)** — the new shared `microsoft`
  connection owns it. Personal accounts use standard consent; some M365 scopes
  need admin consent (read-only calendar generally does not).
- No TCC, no local files. Standalone-clean. CalDAV is an alt path for personal
  Outlook.com but Graph is preferred for full fidelity.

## Vault mapping

- **Raw layer:** `calendar/outlook/raw/YYYY-MM.jsonl` — the Graph event objects,
  full fidelity.
- **Contract layer:** `calendar/outlook/YYYY-MM.jsonl` per the **ratified
  calendar contract**: one row per event (`ts` = start, `source`, `guid` =
  event id, `title`, `start`, `end`, `location`, `organizer`, `attendees[]`,
  `recurrence`, `status`), Graph-specific fields (`onlineMeeting`, categories)
  in `extra`.
- **Dedupe `guid`:** Graph event `id`; delta `$deltaToken` persisted in
  `.trove/outlook-calendar-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/outlook_calendar.rs`: `DEF` (Periodic),
   `pull` hook for Sync-now.
2. New shared `microsoft` `CONNECTION` (OAuth; baked Azure AD app, label/help/
   placeholder per the affordance rule). **The Microsoft To Do def reuses this
   same connection** — design it shared from the start.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`; set
   `connection: Some("microsoft")` on this def and on `microsoft-todo`.
4. Fixtures from Graph docs example responses (single + recurring + cancelled
   events); parser + store + delta-cursor tests, unique temp dirs.
5. Vault writes via `store` helpers against the ratified calendar contract.

## Build notes (2026-06-16)

- Implemented as a `Behavior::Periodic` collector using `GET /me/calendarView/delta` (v1.0 stable) per account.
- Reuses `microsoft` connection from `outlook.rs`; added `Calendars.Read` to `MICROSOFT.scopes` and `"outlook-calendar"` to `CONNECTION.auto_pull`.
- Added `pub(crate) const MICROSOFT_ROW_PREFIX = "mcal:"` to `calendar.rs`; added `pub(crate) fn microsoft_accounts` and `pub(crate) fn microsoft_fresh_token` accessors to `outlook.rs`.
- Raw layer: `calendar/outlook/raw/YYYY-MM.jsonl` (full Graph event JSON, enveloped with `ts` + `account_id`).
- Contract layer: `calendar/events/YYYY-MM.jsonl` via `calendar_snapshot_scoped` with `mcal:{account_id}/` row ownership. First-ever sync baselines silently; subsequent syncs diff + emit change lines.
- Delta cursor: `@odata.deltaLink` persisted per-account in `.trove/outlook-calendar-sync.json`; cursor expiry (HTTP 410) resets to a fresh windowed drain automatically.
- Graph calendarView delta expands recurring occurrences (singleEvents-equivalent); `@removed` annotation handles deletions; `isCancelled` maps to `status: "canceled"`.
- Scope requires user to add `Calendars.Read` to their Azure app registration (setup copy updated).
- 13 tests, all passing. Needs-login (Microsoft Azure app registration + live account to validate).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Events | — | OAuth into a Microsoft account in the connect card; Sync now; confirm rows in `calendar/events/` with `mcal:` prefix + hub last-data |
| Incremental delta | — | sync, add/move an event in Outlook, re-sync; confirm only the changed event updates and no duplicate appears |
| Delta reset | — | Delete `.trove/outlook-calendar-sync.json`; re-sync; confirm all events re-baseline silently |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Microsoft Outlook Calendar (L2530–L2536). Feasibility 🟢 high — Graph is stable
and well documented. Delta query keeps incremental syncs cheap. Bundle the
Azure app registration with Microsoft To Do so one login serves both. EWS dies
Oct 2026 — Graph is the only durable path.
