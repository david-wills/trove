# Microsoft To Do

- **id:** `microsoft-todo`
- **domains:** `tasks/` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (Graph API poll; snapshot diff for the event stream)
- **connection:** `microsoft` — OAuth (Microsoft Entra; delegated `Tasks.Read`).
  **Shared connection:** reuses the same Azure app registration + token as
  Outlook and future Teams / OneNote / OneDrive defs — one login, many defs,
  Google-style.
- **evidence:** official-docs — Microsoft Graph v1.0
  (`/me/todo/lists`, `/me/todo/lists/{id}/tasks`; scope `Tasks.Read`). Stable
  v1.0 surface with a rich task model.
- **effort / priority:** M / P2
- **needs:** Needs-login (validation only — build proceeds from documented
  Graph shapes)

## What it is

Microsoft's task manager (the ex-Wunderlist successor), bundled into the
Microsoft 365 / Outlook ecosystem and surfaced in Outlook, Teams, and Windows.
For anyone living in Microsoft accounts it's the natural to-do store, and it
captures migrated Wunderlist users. Rich model: due dates, reminders,
recurrence, importance, and checklist items.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Lists | personal + work/school | list name → `project` | official Graph docs |
| Tasks | same | title, `bodyContent` notes, `dueDateTime`, `completedDateTime`, `reminderDateTime`, `importance`, `status` | official Graph docs |
| Recurrence | same | `recurrence` pattern → RRULE | official Graph docs |
| Checklist items | same | `checklistItems[]` → `subtasks` | official Graph docs |
| Linked resources | same | `linkedResources` (e.g. originating email) → `extra` | official Graph docs |

All optional in the contract (omit-if-empty); no tier-specific code paths. A
work tenant may deny consent to a personal app — the def surfaces the error
honestly.

## Access & auth

- Graph v1.0: `GET /me/todo/lists`, then `GET /me/todo/lists/{id}/tasks` per
  list. OAuth 2.0 delegated scope `Tasks.Read`; PKCE public client. Personal
  Microsoft accounts work without admin consent. The beta `/me/tasks` endpoint
  exposes additional fields but v1.0 covers the core — stay on v1.0.
- **Shared `microsoft` connection:** registered once (by Outlook); this def
  sets `connection: Some("microsoft")` and rides the existing token. Record
  account identity by stable subject claim, multi-account capable.
- No TCC. Standalone-clean (plain HTTPS). Bring-your-own client_id fallback
  inherited from the shared connection satisfies the built-for-anyone rule.

## Vault mapping

- **Raw layer:** optional `tasks/microsoft-todo/raw/` for verbatim Graph task
  objects if a fidelity toggle is enabled; lean default omits it.
- **Contract layer:** ratified tasks contract —
  `tasks/microsoft-todo/tasks.jsonl` (open-task snapshot, rewritten whole each
  sync) + `tasks/microsoft-todo/events/YYYY-MM.jsonl` (completed/created/
  deleted events, diffed snapshot-to-snapshot). Field mapping: `id` = Graph
  task id, `title`, `project` = list name, `notes` = `bodyContent`, `due` =
  `dueDateTime`, `completed` = `completedDateTime`, `priority` from
  `importance` (low/normal/high → 1/3/5), `recurrence` raw RRULE, `subtasks`
  from `checklistItems`; `linkedResources` and the rest in `extra`.
- **Dedupe:** Graph task id as the snapshot key; deltaLink/`@odata.deltaLink`
  or watermark cursor in `.trove/microsoft-todo-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/microsoft_todo.rs` (or under `sync/`
   following `sync/ticktick.rs`): `DEF` (Periodic, hourly-ish), `pull` hook for
   Sync-now. **No new `CONNECTION`** — set `connection: Some("microsoft")`.
2. Registration line in `INTEGRATIONS` only (connection already registered by
   Outlook).
3. Fixtures from Graph docs example responses (list page, tasks page with
   recurrence + checklist items); parser + store + snapshot-diff tests
   (open→done transition produces a `completed` event), unique temp dirs.
4. Snapshot-diff drives the event stream: To Do exposes open tasks; diff the
   new snapshot against the previous to synthesize completions (never guess
   completed vs. deleted — carry forward and retry, per the contract).
5. Vault writes via the ratified-tasks `store` helpers — no contract wait.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Personal account | Needs-login | OAuth a real outlook.com account; Sync now; confirm rows in `tasks/microsoft-todo/tasks.jsonl` + hub last-data |
| Completion events | Needs-login | complete a task in To Do; re-sync; confirm a `completed` event in `events/` and the task drops from the snapshot |
| Recurrence + checklists | Needs-login | create a recurring task with subtasks; confirm `recurrence` type and `subtasks` land correctly |
| Work/school account | Needs-login | needs a Microsoft 365 work account (any real user's run can validate; tenant consent may block — confirm the error surfaces honestly) |

## Build notes (2026-06-17)

- Built as a Periodic follower; reuses `connection: Some("microsoft")` from `outlook.rs`.
- **Scope gap:** the `microsoft` OAuth Provider in `outlook.rs` currently requests
  `Mail.Read Calendars.Read User.Read offline_access` but NOT `Tasks.Read`. The
  Azure app registration must add `Tasks.Read` and users must reconnect before
  live validation can succeed. Flagged `Needs-David(scope-add)`.
- Graph v1.0 datetime fields (`dueDateTime`, `completedDateTime`, `startDateTime`,
  `reminderDateTime`) are `dateTimeTimeZone` objects `{ "dateTime": "...", "timeZone": "UTC" }`;
  the `dateTime` string is naive UTC (no Z/offset). We normalize to RFC3339 UTC
  then convert to local. `checklistItems.checkedDateTime` is a bare DateTimeOffset
  string, handled separately.
- 14 unit tests; all pass. `cargo check` clean.

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Microsoft To Do (L2546–L2552). Feasibility 🟢 high — stable v1.0 Graph
endpoint, rich model. Reuses the Outlook Azure app registration + token (the
shared-connection design pays off here: zero new auth code). Beta `/me/tasks`
has extra fields but v1.0 is sufficient and stable. Sequence right after
Outlook so the shared `microsoft` connection is already wired.
