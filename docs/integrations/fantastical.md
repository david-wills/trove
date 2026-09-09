# Fantastical

- **id:** `fantastical`
- **domains:** `calendar/` (contract: ✅ **calendar**, ratified) · `tasks/`
  (contract: ✅ **tasks**, ratified)
- **status:** 🧪 built — covered by shipped defs (see validation matrix)
- **unavailable_reason:** none
- **behavior:** CoveredBy(calendar) — no separate collector; the data already
  arrives through the Apple Calendar + Apple Reminders defs.
- **connection:** none — TCC-Calendar is already granted for the shipped Apple
  Calendar def; nothing new to authorize.
- **evidence:** official-shape — Fantastical is a frontend over
  EventKit/CalDAV stores with no proprietary local DB
- **effort / priority:** S / P2
- **needs:** none

## What it is

Fantastical (Flexibits) is a popular Mac/iOS calendar-and-tasks client. It is a
**frontend** over the system calendar/reminder stores and direct CalDAV/
Exchange accounts — it has no proprietary local database of its own. Whatever a
user schedules in Fantastical is the same underlying event/reminder data Trove
already collects through Apple Calendar (EventKit) and Apple Reminders. This
brief exists to answer "why isn't Fantastical its own card?" — because building
one would duplicate data Trove already has.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Calendar events | all | via EventKit / CalDAV (already collected) | official shape |
| Tasks / reminders | all | via Apple Reminders (already collected) | official shape |

No Fantastical-specific fields — it reads and writes the same stores the
shipped defs already read.

## Access & auth

- Fantastical 3+ manages its own CalDAV/Exchange connections by default and
  does **not** require macOS Calendar APIs. **However**, events it manages are
  still reflected in Apple Calendar when the same accounts are added at the
  macOS level — at which point EventKit (already built) sees them. Fantastical
  tasks sync through Apple Reminders.
- Auth: none new. TCC-Calendar is already granted for the Apple Calendar def.
- Standalone-clean: no new code path, no network of our own.

## Vault mapping

- **Calendar:** lands in the shipped Apple Calendar def's output under the
  ratified **calendar** contract — no Fantastical-specific folder.
- **Tasks:** lands in the shipped Apple Reminders def's output under the
  ratified **tasks** contract.
- No `calendar/fantastical/` or `tasks/fantastical/` folder is created;
  routing is by the underlying store, not the client app.

## Build plan

**Nothing to build** — document, don't implement. The `fantastical` entry maps
to `CoveredBy(calendar)` so the hub can explain coverage. The only action is to
ensure the Apple Calendar def's docs note that Fantastical-managed events are
captured when the accounts are added at the macOS level. If a user runs
Fantastical with accounts that are *not* surfaced in Apple Calendar
(Fantastical-only CalDAV), point them at the CalDAV def for that account.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Calendar events | 🧪 built (shipped pre-pipeline via `calendar` def — David promotes to ✅) | add a Fantastical-managed account at the macOS level; confirm its events appear in `calendar/` through the Apple Calendar def |
| Tasks | 🧪 built (shipped pre-pipeline via `apple-reminders` def — David promotes to ✅) | confirm Fantastical reminders appear in `tasks/` through Apple Reminders |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Fantastical (L2706–L2712). Feasibility 🟢 high — no separate integration
needed. Fantastical 3+ does not use macOS Calendar APIs by default (it runs its
own CalDAV), but its events still reflect into Apple Calendar when accounts are
added at the macOS level; tasks sync to Apple Reminders. The research
recommendation is explicitly **skip / document in Apple Calendar docs** rather
than build a standalone connector. Maps to the shipped `calendar` and
`apple-reminders` defs.
