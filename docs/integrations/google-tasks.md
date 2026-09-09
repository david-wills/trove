# Google Tasks

- **id:** `google-tasks`
- **domains:** `tasks/` (contract: **`tasks` — ✅ ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll task lists; completed tasks included)
- **connection:** `google` — OAuth, shared with the five other Google defs
  (Calendar, Contacts, Gmail, YouTube history, Takeout-class pulls). One
  login, full-scope bundle; the `tasks.readonly` scope rides that consent.
- **evidence:** official-docs — tasks.googleapis.com/tasks/v1 (REST,
  documented resources, `tasks.readonly` scope)
- **effort / priority:** S / P2
- **needs:** none

## What it is

Google's lightweight to-do list, surfaced inside Gmail and Google Calendar.
Used by anyone in the Google ecosystem who keeps simple checklists alongside
their mail and calendar. The data matters as one more task stream feeding the
ratified `tasks` timeline — low-fidelity but ubiquitous.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Task lists | free | list id, title | official docs |
| Tasks | free | title, due, status, completed time, position, parent | official docs |
| Completed tasks | free | same + `completed` timestamp | official docs |

Limited model: no notes-as-attachments, no labels, no rich body. All fields
optional in the contract (omit-if-empty); a sparse task simply carries fewer
columns.

## Access & auth

- REST: `GET /tasks/v1/users/@me/lists`, then
  `GET /tasks/v1/lists/{taskList}/tasks` (pass `showCompleted=true`,
  `showHidden=true` to capture done items). Base `tasks.googleapis.com`.
- OAuth via the shared `google` connection; scope
  `https://www.googleapis.com/auth/tasks.readonly`.
- Standard Google Cloud quotas — trivially fine for a personal periodic pull.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `tasks/google-tasks/raw/YYYY-MM.jsonl` — the API task
  objects, full fidelity, partitioned by month.
- **Contract layer:** `tasks/google-tasks/YYYY-MM.jsonl` per the ratified
  `tasks` contract — one row per task (`ts`, `source`, `guid` = task id,
  `title`, `due`, `status`, `completed_at`); list membership and `position`/
  `parent` ordering in `extra`.
- **Dedupe:** task id as `guid`; per-list `updated` watermark cursor in
  `.trove/google-tasks-sync.json`, rebuildable by scanning output files.

## Build plan

Already shipped pre-pipeline as the `google-tasks` def on the shared Google
connection. Remaining work is promotion, not construction:

1. Confirm the existing def writes through the ratified `tasks` `store`
   helper (not a bespoke shape).
2. Verify completed-task capture (`showCompleted`/`showHidden`) against a
   real account.
3. David promotes 🧪 → ✅ once real-data rows land.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Lists + tasks | — | shipped pre-pipeline; with the Google connection live, Sync now; confirm rows in `tasks/google-tasks/` + hub last-data |
| Completed tasks | — | complete a Google Task, Sync now, confirm a `completed_at`-bearing row appears |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Google Tasks (L2538–L2544). Feasibility 🟢 high. Free API, stable, bundles
naturally with the Google Calendar OAuth flow (which is why it shipped on the
shared connection). Limited data model is a known ceiling, not a defect —
covers the core to-do shape and nothing more.
