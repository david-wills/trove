# Todoist

- **id:** `todoist`
- **domains:** `tasks/` (contract: **✅ ratified** — `tasks`)
- **status:** 🧪 built (fixture-tested; TokenPaste personal token; reuses apply_tasks_sync; needs a token to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (poll for new/changed tasks; sync_token watermark)
- **connection:** `todoist` — TokenPaste (personal API token from Settings →
  Integrations → Developer; OAuth also possible but token is simpler for a
  personal pull). Not shared with other defs.
- **evidence:** official-docs — api.todoist.com/api/v1 (unified REST+Sync,
  current/stable; completed tasks via /tasks/completed_by_completion_date)
- **effort / priority:** S / P1
- **needs:** none (connect early — see research note on the paid-plan
  completed-tasks window)

## What it is

One of the most widely used cross-platform task managers: projects, sections,
labels, tasks with due dates, priorities, sub-tasks, reminders, and comments.
High-value as a tasks source — it captures the user's intent stream (what they
meant to do, when, and whether they finished), which the calendar and activity
domains don't.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Active tasks | all plans | content, due, priority, project, labels, parent | official docs |
| Projects / sections / labels | all plans | names, hierarchy, colors | official docs |
| Completed tasks | free = last week only; paid = full history | content, completed_at | official docs |
| Comments / notes | all plans | text, posted_at | official docs |

All optional in the `tasks` contract (omit-if-empty); a free-tier user's
completed-task history simply stops at one week back. No tier-specific code
paths.

## Access & auth

- REST v1: `GET /api/v1/tasks` (active), `GET
  /api/v1/tasks/completed_by_completion_date` (history); Bearer token.
- Sync: `POST /api/v1/sync` with `resource_types=["all"]` and `sync_token=*`
  for a full initial dump (projects, sections, labels, tasks, reminders,
  notes, filters in one call), then incremental syncs with the returned token.
- No published hard rate limit; practical ~50 req/s — trivially fine for a
  periodic personal pull. No TCC, no local files. Standalone-clean (HTTPS).

## Vault mapping

- **Raw layer:** `tasks/todoist/raw/YYYY-MM.jsonl` — the API task/project
  objects, full fidelity.
- **Contract layer:** `tasks/todoist/YYYY-MM.jsonl` per the ratified `tasks`
  contract — one row per task (`ts` = created/completed, `source`, `guid` =
  task id, `title` = content, `status`, `due`, `priority`, `project`,
  `labels[]`), overflow (sub-task parent ids, comment text, reminders) in
  `extra`.
- **Dedupe:** task id as `guid`; sync_token cursor in
  `.trove/todoist-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/todoist.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: label/help/placeholder per the SimpleFIN affordance rule),
   `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the official docs example responses (active tasks, completed
   tasks, full sync dump); parser + store + cursor tests, unique temp dirs.
4. Map to the ratified `tasks` contract via the shared `store` helpers.

## Build status — 🧪 2026-06-14

Shipped (`todoist.rs`, INDEX #13 — a *later* collector in the already-bound
`tasks` domain, so no binding). `Behavior::Periodic` (15 min). Reuses
`apply_tasks_sync` (the github tasks-leg / ticktick precedent).

Auth (`CONNECTION` = `todoist`, TokenPaste): a **personal API token** (Settings →
Integrations → Developer), stored 0600 via `save_sync_token` (a secret — never in
the cursor/logs). **Todoist API v1** (REST v2 is deprecated), `Authorization:
Bearer`.

Pull: `GET /api/v1/projects` + `GET /api/v1/tasks` (active; drains `next_cursor`
pagination, items under `results`) → `Vec<Task>` → `apply_tasks_sync("todoist",
projects, fresh, fate)`. The `fate` closure fetches `GET
/api/v1/tasks/completed/by_completion_date` (items under `items`) once per pull:
a vanished task that's completed → `Completed(completed_at → local)`; absent →
`Deleted`; on fetch error → `Unknown` (carry forward). Map: `content`→title,
`project_id`→project NAME, **priority 1→0/2→1/3→3/4→5** (API 4=highest; raw kept
in `extra.todoist_priority`), `due` datetime‖date → local + `all_day`,
`recurrence` from `due.string`, `labels`→tags, **`added_at`**→created,
parent_id/section_id/url → `extra`. Raw `tasks/todoist/raw/`; cursor
`.trove/todoist-sync.json`.

Evidence: the v1 shapes were verified against the official Doist TS SDK + the
CnTeng Go SDK (the HTML docs are JS-rendered). Caught: the creation field is
`added_at` (not `created_at`); active wraps under `results`, completed under
`items`, both with `next_cursor`.

Adversarial-verify confirmed the mapping/priority/fate/pagination/token/serde all
correct; 0 blocking + 3 minor — fixed the completed-window `since`/`until` to
ISO8601-UTC with a `Z` suffix (matches the SDK); accepted the cosmetic fallback
`url` (the server sends the real one) + the `is_deleted` defensive case (the v1
active endpoint never returns deleted/checked items).

**Deferred:** comments/reminders/sub-task depth (parent_id rides `extra`),
Sync-API incremental (full-snapshot REST for v1).

Gate: trove-core 557/0 (+19 todoist tests), `cargo check` clean, `schedule_doc`
regenerated (todoist Periodic), `bindings.ts` up to date. **Live token →
Needs-login.**

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Active tasks + projects | 🧪 (Needs-login) | paste a real token (Settings → Integrations → Developer) in the connect card; Sync now; confirm rows in `tasks/todoist/` + the Tasks view |
| Completed history | 🧪 (Needs-login) | confirm the last-week window on a free plan; a paid account confirms full history (any real user's run); completed tasks appear as `completed` events |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Todoist (L2514–L2520). Feasibility 🟢 high. REST v2 is **deprecated** — use
v1 (unifies REST and Sync). Completed tasks beyond ~1 week require a paid
Todoist plan, so connect early to start banking history. Sequence alongside
the other ratified-`tasks` sources (Linear, Asana, Trello) to exercise the
contract with multiple shapes.
