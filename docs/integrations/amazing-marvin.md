# Amazing Marvin

- **id:** `amazing-marvin`
- **domains:** `tasks/` (contract: ✅ **ratified** — tasks) · `habits/`
  (contract: **Phase 3 pending** — habits)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll tasks/projects/habits; watermark cursor)
- **connection:** `amazing-marvin` — TokenPaste (API key + token from Amazing
  Marvin Settings → API; **requires an active subscription** to enable the
  API). Not shared with other defs.
- **evidence:** official-docs — serv.amazingmarvin.com/api (documented REST
  endpoints) · community-schema — `bgheneti/Amazing-Marvin-MCP` (28-tool MCP
  server, good reference for the API surface)
- **effort / priority:** S / P2
- **needs:** Needs-login (subscription-gated API — must surface the
  subscription requirement in the connect-card copy) · habits contract not
  yet ratified (Needs-David, for the habits slice only — the tasks slice is
  unblocked)

## What it is

Amazing Marvin is a highly customizable to-do/productivity app with an
enthusiastic ADHD/power-user base. It models tasks, projects, daily lists
("dailies"), and habits with streaks. For Trove it's a tasks source plus a
habits source — the task graph routes to the ratified tasks contract, while
habit streaks and check-in history route to `habits/`. The subscription gate
on the API limits the audience but the data is rich for those who have it.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Today items | subscription (API) | tasks due/scheduled today | official docs |
| Tasks | subscription (API) | task title, done state, due/schedule dates, project | official docs |
| Projects | subscription (API) | project/category structure | official docs |
| Habits | subscription (API) | habit definitions | official docs |
| Habit streaks + check-ins | subscription (API) | streak data, check-in history (scoring/checkin endpoints separate) | community (MCP ref) |

All optional in the contract (omit-if-empty); a user with no habits simply
yields no `habits/` rows.

## Access & auth

- REST: `GET https://serv.amazingmarvin.com/api/todayItems`, `/api/tasks`,
  `/api/habits` (scoring/checkin endpoints are separate). API key from
  Amazing Marvin Settings → API.
- Auth: TokenPaste — the API key/token is only available to **active
  subscribers**; the connect card must say so plainly (disabled-control
  affordance rule) so a free-tier user understands why it's gated.
- Rate limits: not prominently documented — be conservative on the periodic
  poll. No TCC, no local files. Standalone-clean (plain HTTPS).
- The MCP server is a documentation reference for the API surface, not a
  runtime path for the compiled-in collector.

## Vault mapping

- **Raw layer:** `tasks/amazing-marvin/raw/YYYY-MM.jsonl` and
  `habits/amazing-marvin/raw/YYYY-MM.jsonl` — the API objects at full
  fidelity, split by shape.
- **Contract layer:**
  - `tasks/amazing-marvin/…` per the ratified **tasks** contract — one row
    per task (`ts`, `source`, `guid` = Marvin task id, `title`, `done`,
    `due`, `project`), overflow in `extra`.
  - `habits/amazing-marvin/…` per the (pending) **habits** contract —
    habit definitions plus check-in history; streak data in `extra`.
  - Records route by shape: tasks → `tasks/`, habits → `habits/`, never
    split across a single record.
- **Dedupe:** Marvin object id as `guid` per stream; cursor in
  `.trove/amazing-marvin-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/amazing_marvin.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: label/help/placeholder that names the
   subscription requirement per the affordance rule), `pull` hook for
   Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented endpoints + MCP reference shapes (tasks,
   projects, habits, a check-in history sample); parser + store + cursor
   tests, unique temp dirs.
4. The tasks slice ships against the ratified contract immediately; the
   habits slice is **parked behind Needs-David (habits contract)** — until
   then write habits raw-only under `habits/amazing-marvin/raw/`.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Tasks / projects | — | paste a real subscriber API key in the connect card; Sync now; confirm task rows in `tasks/amazing-marvin/` + hub last-data |
| Habits + streaks | — | requires an active subscription with habits configured; Sync; confirm habit/check-in rows under `habits/amazing-marvin/` (any real subscriber's run validates this) |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Amazing Marvin (L2658–L2664). Feasibility 🟡 medium. API is documented and
covers tasks/projects/habits/dailies but is gated behind an active
subscription — niche but enthusiastic audience. The `bgheneti/Amazing-Marvin-MCP`
server (28 tools incl. get_daily_productivity_overview, get_all_tasks) is a
strong reference for the full API surface. Habits carry streak + check-in
history — valuable once the habits contract lands.
