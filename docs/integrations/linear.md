# Linear

- **id:** `linear`
- **domains:** `tasks/` (contract: **`tasks` ✅ ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll assigned issues; watermark on `updatedAt`)
- **connection:** `linear` — TokenPaste (personal API key from Settings >
  Account > Security & Access; no OAuth dance for personal use). Not shared
  with other defs.
- **evidence:** official-docs — Linear GraphQL API
  (`api.linear.app/graphql`), documented `me { assignedIssues }` + cursor
  pagination
- **effort / priority:** S / P1
- **needs:** Needs-login (validation only — build proceeds from documented
  shapes)

## What it is

Linear is a fast, opinionated issue tracker popular with software teams and
indie developers. The personal value is the user's own assigned work:
issues with status, cycle, project, labels, and comment history. Trove
pulls the assigned-work slice via the personal API key.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Assigned issues | all plans | id, identifier, title, state, priority, dueDate, project, cycle, createdAt/updatedAt | official docs |
| Comments | all plans | body, author, createdAt (per issue) | official docs |
| Cycles / projects | all plans | name, dates, progress (context for issues) | official docs |

All optional in the contract (omit-if-empty); no tier-specific code paths.
Read-only key scope is sufficient.

## Access & auth

- GraphQL: `POST https://api.linear.app/graphql`; query
  `me { assignedIssues { nodes { ... } } }`. Auth via personal API key
  header (key scopes are Read / Write / Admin — **Read is enough**).
- Pagination: cursor (`pageInfo.endCursor` / `after`). Rate limit
  ~100–300 req/min per token — trivially fine for a periodic personal pull.
- No TCC, no local files. Standalone-clean (plain HTTPS). Personal API key
  needs no app registration. Some queries need team/workspace context — the
  `me { assignedIssues }` root avoids needing the user to pick a team.

## Vault mapping

- **Raw layer:** `tasks/linear/raw/YYYY-MM.jsonl` — the GraphQL issue nodes,
  full fidelity (preserves comments, labels, cycle/project refs).
- **Contract layer:** `tasks/linear/YYYY-MM.jsonl` per the ratified `tasks`
  contract — one row per issue (`ts` from `updatedAt`, `source`, `guid` =
  issue id, `title`, `status` = state name, `due` = dueDate,
  `completed_at`, `project`); priority/labels/cycle and overflow in `extra`.
  Assigned-work shape routes here whole.
- **Dedupe:** issue id as `guid`; cursor (max seen `updatedAt`) in
  `.trove/linear-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/linear.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: API-key field, label/help/placeholder per the SimpleFIN
   affordance rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from GraphQL `me { assignedIssues }` example responses (with +
   without comments); parser + store + cursor tests, unique temp dirs.
4. Vault writes via `store` helpers against the ratified `tasks` contract.

## Build notes

- **Behavior:** `Periodic` (every 15 min), polling `me { assignedIssues }` with
  `updatedAt_gte` watermark so only changed issues are re-fetched after the
  first sync.
- **Contract:** `tasks` (reuse-bound) — `apply_tasks_sync` with a fate map built
  from completed/cancelled issues returned in the `updatedAt` window.
- **Raw layer:** `tasks/linear/raw/YYYY-MM.jsonl` — full GraphQL nodes including
  comments, labels, cycle refs; upserted by `id`, partitioned by `createdAt`.
- **Fate:** completed and cancelled issues arrive in the same pull (their
  `updatedAt` advances when closed), so they go into a `completed_map` before
  the diff. Vanished-from-active + not-in-completed-map → Deleted.
- **Priority mapping:** Linear 0=none/1=Urgent/2=High→5, 3=Medium→3, 4=Low→1.
  Raw `linear_priority` preserved in `extra`.
- **Due dates:** Linear only has date-level dues (no time), so all duet values
  set `all_day: true`.
- **Connection:** `linear` (NEW TokenPaste; needs `&crate::linear::CONNECTION`
  added to `CONNECTIONS` in `integrations.rs` — done in this branch).
- **21 tests pass,** cargo check clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Assigned issues | 🧪 built | paste a personal API key in the connect card; Sync now; confirm rows in `tasks/linear/` + hub last-data |
| Comments / cycle context | 🧪 built | confirm comments and cycle/project refs land in `extra` for an issue that has them |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Linear (L2562–L2568). Feasibility 🟢 high. Personal API key is trivial to
obtain and needs no app registration. Assigned-work shape routes to
`tasks/`; the platform-activity firehose (events/webhooks) is out of scope
— Trove wants the user's own issues, not the workspace stream. Shares the
ratified `tasks` contract with TickTick, Jira, Todoist, Reminders.
