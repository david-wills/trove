# Motion

- **id:** `motion`
- **domains:** `tasks/` (contract: ✅ ratified) · `calendar/` (contract:
  ✅ ratified) — Motion's auto-scheduled task slots are calendar events
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (REST poll; snapshot diff for the task event stream)
- **connection:** `motion` — TokenPaste (API key from Motion Settings → API
  Keys; **requires a paid plan**). Not shared with other defs.
- **evidence:** official-docs — `api.usemotion.com/v1` (`/v1/tasks`,
  `/v1/projects`, `/v1/schedules`); unofficial `RF-D/motion-mcp` GitHub
  project as a reference for the response shapes.
- **effort / priority:** S / P2
- **needs:** Needs-login (paid Motion plan required to mint an API key — the
  connect card must surface that gate honestly, per the disabled-control
  affordance rule)

## What it is

Motion is an AI calendar/task app: you add tasks with durations, deadlines,
and priorities, and it auto-schedules them into open calendar slots. The
value is the *scheduled* layer — when Motion decided to do each task — on top
of the task list itself. Growing user base; broad deployment is limited by the
paid-plan API gate.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Tasks | paid plan (API) | name, description, `dueDate`, `duration`, `priority`, `status`, `completed` | official docs |
| Projects | paid plan | project name → `project` | official docs |
| Schedules | paid plan | auto-scheduled task time slots | official docs |
| Appointments | — | **not exposed via API** | official docs |

All optional in the contract (omit-if-empty). No tier-specific code paths —
without a key the def is gated and reports no data. Appointment-scheduling
data is simply unavailable; the def never invents it.

## Access & auth

- REST: `GET https://api.usemotion.com/v1/tasks`, `/v1/projects`,
  `/v1/schedules`. Auth is an API key minted at Motion Settings → API Keys,
  pasted into the connect card. **API access requires a paid subscription** —
  free users have no key, so the connect button stays gated with an inline
  hint ("Motion API needs a paid plan").
- No TCC, no local files. Standalone-clean (plain HTTPS).
- Data export is available before cancellation — note for users who churn off
  the paid plan but want a final pull.

## Vault mapping

- **Raw layer:** optional `tasks/motion/raw/` for verbatim task/schedule
  objects if a fidelity toggle is on; lean default omits it.
- **Contract layer (tasks):** ratified tasks contract —
  `tasks/motion/tasks.jsonl` (snapshot) + `tasks/motion/events/YYYY-MM.jsonl`
  (events via snapshot diff). Field mapping: `id` = Motion task id, `title` =
  name, `notes` = description, `due` = `dueDate`, `priority` mapped onto the
  0/1/3/5 scale, `completed`; duration and labels in `extra`.
- **Contract layer (calendar):** the `/v1/schedules` auto-scheduled slots are
  calendar-shaped (a task placed into a time block) and route **whole** to
  `calendar/motion/` per the ratified calendar contract — one event per
  scheduled slot, linking back to the task id in `extra`. (Records route by
  shape: the task is a task, the scheduled block is a calendar event.)
- **Dedupe:** Motion task id as the snapshot key; schedule-slot id as the
  calendar `guid`; cursor in `.trove/motion-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/motion.rs`: `DEF` (Periodic), `CONNECTION`
   (`motion`, TokenPaste — label/help/placeholder per the affordance rule,
   stating the paid-plan requirement), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from official docs example responses (tasks page, projects,
   schedules); parser + store + snapshot-diff tests for completion events,
   unique temp dirs.
4. Snapshot-diff drives task completions (never guess completed vs. deleted —
   carry forward and retry, per the contract).
5. Gate UX: when no key is present, the connect button is disabled with an
   inline hint about the paid-plan requirement.
6. Vault writes via the ratified tasks + calendar `store` helpers — no
   contract wait.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Tasks | — | paste a real API key (paid plan); Sync now; confirm rows in `tasks/motion/tasks.jsonl` + hub last-data |
| Completion events | — | complete a task in Motion; re-sync; confirm a `completed` event and the task drops from the snapshot |
| Schedules → calendar | — | confirm auto-scheduled slots land in `calendar/motion/` with the right times, linked to their task id |
| Paid-plan gate | — | with no key, confirm the connect button reads disabled with the paid-plan hint |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Motion (AI Scheduler) (L2618–L2624). Feasibility 🟡 medium — official REST
API covers tasks/projects/schedules, but the paid-plan gate limits broad
deployment (hence P2). Appointment-scheduling is explicitly **not** in the
API. The unofficial `RF-D/motion-mcp` project documents the response shapes
as a reference. Export is available pre-cancellation for users winding down.
