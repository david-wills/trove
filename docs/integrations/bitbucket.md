# Bitbucket

- **id:** `bitbucket`
- **domains:** `developer/` (raw-only — heterogeneous shapes, no contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (incremental pull; per-repo commit cursors)
- **connection:** `bitbucket` — TokenPaste (App Password from account
  settings; simplest for personal use) with OAuth 2.0 as a possible later
  method. Not shared with other defs.
- **evidence:** official-docs — Atlassian Bitbucket Cloud REST API v2 at
  api.bitbucket.org/2.0/ (well-documented, but no user activity events feed)
- **effort / priority:** S / P2
- **needs:** Needs-login (validation only — build proceeds from documented
  shapes)

## What it is

Atlassian's git hosting platform. Declining market share versus
GitHub/GitLab but still common in Atlassian-shop workplaces, so some users'
"what did I build" history lives only here. Yields commits and pull
requests the local-git collector misses (repos never cloned to this Mac,
PR/review activity).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Repositories | all plans | repo slug, workspace, description, updated_on | official API v2 docs |
| Commits | all plans | sha, ts, message, repo, author | official API v2 docs |
| Pull requests | all plans | id, title, state, created/updated ts, source/dest branch | official API v2 docs |

All optional in the per-source shape; no tier-specific code paths.

## Access & auth

- REST API v2 at `api.bitbucket.org/2.0/`. App Password (user-specific
  token, created in account settings) via Basic auth; OAuth 2.0 exists but
  App Password is the simpler personal-use path. v1 is fully deprecated.
- Key endpoints: `/repositories/{username}` (own repos),
  `/repositories/{workspace}/{slug}/commits` (per-repo; filter by author),
  `/pullrequests?q=author.uuid=...`.
- **No user activity events feed** (unlike GitHub `/user/events`) — the
  collector must iterate repos to find commits, which costs more requests.
  Cursor-based pagination (`next` URL in each response).
- Self-hosted Bitbucket Data Center has a separate REST API — out of scope
  for v1; note it in the def description.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `developer/bitbucket/YYYY-MM.jsonl` — same record schema
  as the GitHub/GitLab platform pulls (commits + PRs as typed rows), per
  the developer-platforms convention in the research doc's cross-cutting
  notes. `developer/` is raw-only per the taxonomy; no contract applies.
- **Dedupe:** commit sha / PR id as `guid`; per-repo highest-imported
  commit timestamp in `.trove/bitbucket-sync.json`, rebuildable from
  output files.

## Build plan

1. Build in the "developer platforms" batch **after GitHub and GitLab** —
   it reuses their vault schema and most of the pull scaffolding.
2. Module `crates/trove-core/src/bitbucket.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: App Password label/help per the SimpleFIN
   affordance rule), `pull` hook iterating repos with cursor pagination.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from the official API docs' example responses (repo list,
   commits page, PR page, pagination `next` link); parser + store + cursor
   tests with unique temp dirs.
5. Watch request volume on first sync (repo iteration); page politely and
   persist per-repo cursors so re-runs are cheap.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Repos + commits | ✅ built (Needs-login) | paste `username:ATBB-xxxx` in the connect card; Sync now; confirm rows in `developer/bitbucket/commits/` + hub last-data |
| Pull requests | ✅ built (Needs-login) | same run; confirm PR rows in `developer/bitbucket/prs/` for an account that has authored PRs |

## Build notes (2026-06-16)

- Behavior: Periodic (hourly), raw-only (developer/ domain).
- Connection: NEW `bitbucket` TokenPaste — credential is `username:ATBB-xxxx` stored as Basic Auth composite.
- API field names confirmed from live api.bitbucket.org responses: commits use `hash` (not sha), `date`, `author.raw`, `author.user.{nickname,account_id}`; PRs use `created_on`/`updated_on`, `source.branch.name`, `destination.branch.name`, `author.{display_name,nickname}`; pagination via top-level `next` URL.
- Commit discovery: iterate `/user/permissions/workspaces` → `/repositories/{ws}?role=member` → `/repositories/{ws}/{slug}/commits`; client-side filter by `author.user.nickname == username` (API `author=` param matches by name string, not account, so it's unreliable).
- PR discovery: `/pullrequests?role=author` account-level endpoint returns all PRs across all repos — more efficient than repo iteration.
- Cursor: per-stream watermarks in `.trove/bitbucket-sync.json`; advance only after full drain; upsert-into-partition by guid prevents duplicates on overlapping windows.
- 12 tests: mapping (confirmed fields), cursor round-trip, pull_with integration via mock, idempotent resync.

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Bitbucket
Activity (L1420–L1426). Feasibility 🟡 medium — API is solid but the
missing events feed makes commit discovery more expensive than
GitHub/GitLab. Cross-cutting note 5 (L1478): GitHub/GitLab/Bitbucket share
one vault output schema; GitHub's archive-ZIP backfill pattern has no
Bitbucket equivalent documented in the research — incremental pull only
for v1.
