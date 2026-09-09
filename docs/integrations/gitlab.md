# GitLab

- **id:** `gitlab`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes);
  `tasks/` (contract: **✅ ratified** — assigned issues are assigned-work
  shape)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (incremental pull off the events feed; per-stream
  watermark cursors)
- **connection:** `gitlab` — TokenPaste (PAT with `read_api`, `read_user`
  scopes; self-hosted instance URL is a configurable field on the same
  connection). Not shared with other defs.
- **evidence:** official-docs — GitLab REST API v4 (gitlab.com/api/v4/ or
  self-hosted base URL); same well-documented pattern as GitHub
- **effort / priority:** S / P1
- **needs:** Needs-login (validation only — build proceeds from documented
  shapes)

## What it is

The second developer platform after GitHub: commits, merge requests, issues,
and the user activity event feed, for both gitlab.com and self-hosted
instances. Captures the "what did I build and review" trail for work that
never touches a locally cloned repo. Self-hosted support is a meaningful
differentiator for users whose employer runs their own GitLab.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Activity events | all plans | push/comment/MR/issue events w/ ts | official docs (`GET /events`) |
| Commits | all plans | sha, ts, message, project, stats | official docs (`/projects/{id}/repository/commits?author=`) |
| Merge requests | all plans | iid, title, state, ts, project | official docs (`/merge_requests?scope=created_by_me`) |
| Issues (authored) | all plans | iid, title, state, ts, labels | official docs (`/issues?scope=created_by_me`) |
| Issues (assigned) | all plans | task rows: title, due, done state | official docs (assigned scope) |

All optional in the contract layer; no tier-specific code paths.

## Access & auth

- REST API v4 at `https://gitlab.com/api/v4/` or the user-configured
  self-hosted URL. PAT in `PRIVATE-TOKEN` header; scopes `read_api` +
  `read_user`.
- `/events` returns recent events (last 100 by default) and takes
  `after`/`before` params for incremental pulls; for full commit history,
  iterate `/projects?membership=true` then `/repository/commits` per
  project. Pagination via `X-Next-Page` header.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `developer/gitlab/YYYY-MM.jsonl` — events, commits, MRs,
  authored issues, same schema as the GitHub module (cross-cutting note 5:
  GitHub/GitLab/Bitbucket share one vault output shape).
- **Contract layer:** issues *assigned to the user* route to `tasks/gitlab/`
  per the ratified tasks contract (assigned-work shape → `tasks/`; the
  activity firehose stays in `developer/`). `guid` = issue global id;
  overflow (labels, milestone, weight) in `extra`.
- **Dedupe:** event id / commit sha / MR global id as `guid` per stream;
  per-stream cursors in `.trove/gitlab-sync.json`, rebuildable from output.

## Build plan

1. Module `crates/trove-core/src/gitlab.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste with instance-URL field; setup copy explains PAT creation
   under User Settings → Access Tokens, per the disabled-affordance rule).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Reuse/mirror the GitHub module's vault schema and pull skeleton —
   sequence this right after GitHub so the shared shape is exercised twice.
4. Fixtures from the documented v4 response shapes (events, commits, MRs,
   issues); pagination + cursor tests, unique temp dirs.
5. Tasks rows through the ratified tasks-contract `store` helpers.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Events/commits/MRs | ✅ built, needs live test | paste a gitlab.com PAT; Sync now; confirm rows in `developer/gitlab/commits/`, `developer/gitlab/issues/`, `developer/gitlab/mrs/` + hub last-data |
| Assigned issues → tasks | ✅ built, needs live test | assign an open issue to the account; Sync now; confirm task row in `tasks/gitlab/tasks.jsonl` |
| Self-hosted instance | ✅ built, needs live test | prefix the PAT with the instance URL (e.g. `https://gitlab.mycompany.com glpat-…`); the connect flow verifies via `/api/v4/user` |

## Build notes (2026-06-15)

- Replaced NotWired stub with full Periodic implementation.
- Raw-only domain (`developer/`) + tasks contract reuse via `apply_tasks_sync`.
- New TokenPaste `CONNECTION` (id="gitlab") added to CONNECTIONS registry.
- Composite credential: `<instance-url> <PAT>` pasted as one string; gitlab.com
  PAT can be pasted alone (ambient_weather.rs composite pattern).
- Commits use `id` (SHA), `authored_date`, `committed_date` (GitLab v4 field
  names differ from GitHub's `sha` / `commit.author.date`).
- Pagination via `X-Next-Page` page-number header; reconstructed into absolute
  URL via `set_page_param`.
- Issues: `scope=all` (includes authored + assigned); `scope=assigned_to_me` for
  task contract. `due_date` is `YYYY-MM-DD` → converted to RFC3339 local in task.
- MRs: `scope=created_by_me`. GitLab v4 `/merge_requests` returns `merged_at` not
  present on issues.
- 8 tests, all green. No new Cargo deps (all already in tree).

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §GitLab
Activity (L1348–L1354). Feasibility 🟢 high. Same pattern and effort as the
GitHub connector; build as part of the developer-platforms batch (GitHub →
GitLab → Bitbucket). GitLab project export exists as an M1 backfill path if
the events window proves too shallow (cross-cutting note 5).
