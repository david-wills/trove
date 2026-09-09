# Asana

- **id:** `asana`
- **domains:** `tasks/` (contract: **✅ ratified** — tasks)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll assigned tasks; watermark on modified time)
- **connection:** `asana` — TokenPaste (Personal Access Token from the Asana
  Developer Console; OAuth 2.0 is an alternative path but PAT is frictionless
  and instant). Not shared with other defs.
- **evidence:** official docs — `app.asana.com/api/1.0/tasks`, PAT instant,
  documented rate limits
- **effort / priority:** S / P1
- **needs:** none

## What it is

Asana is a widely-used task / project-management service common in
professional teams. Capturing assigned tasks (open and completed) gives a
durable personal record of work — what was on your plate and when it closed —
independent of the team's Asana account.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Assigned tasks | all plans | name, notes, due/completed dates, assignee, projects, sections, tags, followers | official docs |
| Completed history | all plans | same fields, `completed=true` | official docs |
| Workspaces | all plans | workspace GIDs (to enumerate) | official docs |

All capability fields are optional in the contract (omit-if-empty); tiering
needs no special code paths.

## Access & auth

- REST: `GET https://app.asana.com/api/1.0/tasks?assignee=me&workspace={gid}`;
  workspace GIDs from `GET /workspaces`. Use `opt_fields` to request the
  specific fields needed; `completed=true` (or `completed_since`) for history.
- Auth: Personal Access Token (Developer Console → My Apps → Personal Access
  Token; no app registration) as a Bearer token, or OAuth 2.0.
- Rate limit: 150 req/min per app/user combo — ample for a periodic personal
  pull.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `tasks/asana/raw/YYYY-MM.jsonl` — the API task objects for the
  requested `opt_fields` set (gid, name, notes, due/start dates, assignee,
  projects, tags, followers, memberships, workspace, permalink_url,
  resource_type). Asana v1 has no `opt_fields=*` wildcard, so this is the
  fidelity of the chosen subset, not all documented task fields.
- **Contract layer:** `tasks/asana/YYYY-MM.jsonl` per the **ratified tasks
  contract** — one row per task (`ts`, `source` = `asana`, `guid` = task GID,
  `title` = name, status/completed, due/start date, project/section/tags),
  overflow (followers, notes, custom fields) in `extra`.
- **Dedupe:** task GID as `guid`; cursor on `completed_since`/modified
  watermark in `.trove/asana-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/asana.rs`: `DEF` (Periodic, hourly-ish),
   `CONNECTION` (TokenPaste: label/help/placeholder per the SimpleFIN
   affordance rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Enumerate workspaces via `GET /workspaces`, then iterate
   `tasks?assignee=me&workspace={gid}` across all of them; request a fixed
   `opt_fields` set.
4. Fixtures from documented example responses (open-task AND completed-task
   variants, multi-workspace); parser + store + cursor tests, unique temp
   dirs.
5. Vault writes via the `store` helpers against the ratified tasks contract —
   no contract wait.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Assigned tasks | ✅ built | paste a real PAT in the connect card; Sync now; confirm task rows under `tasks/asana/` + hub last-data |
| Completed history | ✅ built | confirm closed tasks appear with completion dates (`completed=true` path via `completed_since`) |
| Multi-workspace | ✅ built | unit tests: `empty_workspace_does_not_delete_prior_tasks` + `multi_workspace_deletion_only_from_non_empty_workspace`; live: with >1 workspace, confirm tasks from each land and empty-workspace tasks carry forward |

## Implementation notes (built 2026-06-15, updated 2026-06-15)

- Pagination: Asana uses offset-based pagination (`next_page.offset`, not a
  cursor string); we pass `limit=100` and drain all pages before the first
  vault write.
- Fate closure: unlike Todoist (which needs a second `/completed` fetch),
  Asana returns completed tasks in the *same* `GET /tasks?completed_since=…`
  call, so the fate closure reads directly from the completed_map built in
  the same pass — no second network round-trip.
- Multi-workspace fate guard: Asana returns 200+`[]` for a workspace with no
  currently-assigned tasks. To avoid false deletions, the fate closure only
  marks a task Deleted if its workspace returned ≥1 task this cycle. Tasks
  from workspaces that returned empty carry forward as Unknown — the same
  pattern as Todoist's `None`-guard for failed completed-window fetches. The
  workspace GID is stored in `extra["workspace_gid"]` to associate previously-
  seen tasks with their workspace on re-read.
- Start date: `start_at` (timed) and `start_on` (date-only) are fetched via
  `opt_fields` and mapped to `Task.start`, parallel to `due_at`/`due_on`.
- Priority: Asana's numeric priority is an enterprise-only field in the REST
  v1 API. The contract `priority` field is always 0; source-specific metadata
  (section, project membership, followers) goes in `extra`.
- 23 module tests covering: parse_page, pagination drain, task mapping (timed
  + all-day + completed + start_at + start_on), fate resolution matrix (incl.
  Unknown from empty workspace), raw dedup, full pull round-trip, completion
  event with correct timestamp, deletion detection, multi-workspace carry-
  forward guard, cursor back-compat, connection/disconnect/0600 file, empty-
  token rejection.

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity" §Asana
(L2586–L2592). Feasibility 🟢 high — stable REST API, PAT is instant, full task
history and project membership available. Build-now candidate: lands on the
already-ratified tasks contract with a frictionless connection, so it's a good
early exerciser of the tasks contract alongside TickTick/Todoist/Linear.
