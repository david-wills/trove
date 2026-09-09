# Jira

- **id:** `jira`
- **domains:** `tasks/` (contract: **`tasks` ✅ ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll assigned/reported issues; watermark on `updated`)
- **connection:** `jira` — TokenPaste (Atlassian email + API token, HTTP
  Basic auth; user also supplies their site domain). Not shared with other
  defs.
- **evidence:** official-docs — Jira Cloud REST API v3
  (`{domain}.atlassian.net/rest/api/3/search/jql`), token-pagination (`nextPageToken`/`isLast`)
- **effort / priority:** M / P1
- **needs:** Needs-David (site domain — the `{domain}` subdomain is
  per-user, must be collected at connect time) · Needs-login (validation
  only — build proceeds from documented shapes)

## What it is

Atlassian's issue tracker / project-management tool, ubiquitous in
engineering and corporate teams. The personal value is the user's own
work: issues assigned to or reported by them, with status, priority, due
dates, and history. Trove pulls the assigned-work slice, not the whole
firehose of a workspace.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Assigned issues | all plans | key, summary, status, assignee, reporter, priority, due date, project, created/updated | official docs |
| Reported issues | all plans | same shape, JQL `reporter=currentUser()` | official docs |
| Issue detail | all plans | description, labels, comments (via expand) | official docs |

All optional in the contract (omit-if-empty); no tier-specific code paths.

## Access & auth

- REST: `GET /rest/api/3/search/jql?jql=assignee=currentUser()` (and
  `reporter=currentUser()`); HTTP Basic auth = email + API token. Base URL
  is `https://{domain}.atlassian.net` — `{domain}` is per-user and must be
  captured in the connect card alongside the token. The legacy
  `/rest/api/3/search` endpoint was deprecated 2024-10-31 and fully removed
  by 2025-10-31; this module targets the replacement exclusively.
- Pagination: token-based via `nextPageToken` query param; response envelope
  has `isLast` (bool) and `nextPageToken` (string, absent on last page). No
  `total` or `startAt` fields. Explicit `fields=` required (default is id-only).
  Rate limits apply per user — fine for a periodic personal pull.
- No TCC, no local files. Standalone-clean (plain HTTPS). API token from
  Atlassian Account Settings > Security > API tokens; no OAuth app
  registration needed for personal use.
- Jira Server / Data Center uses a different base URL and an older API
  version — **target Cloud first**; Server is out of scope for v1.

## Vault mapping

- **Raw layer:** `tasks/jira/raw/YYYY-MM.jsonl` — the API issue objects,
  full fidelity (preserves description, comments, labels).
- **Contract layer:** `tasks/jira/YYYY-MM.jsonl` per the ratified `tasks`
  contract — one row per issue (`ts` from `updated`, `source`, `guid` =
  issue key, `title` = summary, `status`, `due`, `completed_at`,
  `project`); priority/labels/reporter and any overflow in `extra`.
  Assigned-work shape routes here whole.
- **Dedupe:** issue key as `guid`; cursor (max seen `updated`) in
  `.trove/jira-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/jira.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: email + token + site-domain fields, label/help/placeholder
   per the SimpleFIN affordance rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from REST v3 `/search/jql` example responses (assigned + reported
   variants, multi-page token pagination); parser + store + cursor tests, unique temp dirs.
4. Vault writes via `store` helpers against the ratified `tasks` contract.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Assigned/reported issues | built (needs live test) | paste three lines (domain/email/token) in the connect card; Sync now; confirm rows in `tasks/jira/` + hub last-data |
| Issue detail | built (needs live test) | confirm labels/issuetype/reporter/priority land in `extra`; description (ADF) is in the raw layer |

## Implementation notes

- Composite TokenPaste credential: three lines — domain / email / API token. Stored in `.trove/sync/jira` (0600) as `token_type`=domain, `scope`=email, `access_token`=token (CalDAV pattern).
- HTTP Basic auth = `base64(email:token)`.
- Two JQL queries per sync (assignee + reporter), deduplicated by issue key. Watermark on max `fields.updated` seen, stored in `.trove/jira-sync.json`.
- Pagination via `nextPageToken` token cursor (new `/search/jql` endpoint); `isLast=true` or absent `nextPageToken` terminates the drain. No `total`/`startAt` used.
- Jira timestamps use `"YYYY-MM-DDTHH:MM:SS.mmm+HHMM"` (no colon in offset, milliseconds). `normalize_jira_ts` normalizes to RFC3339 before `to_local`.
- Raw layer partitioned by `fields.created` month, upserted by issue key.
- Contract layer uses `apply_tasks_sync`; fate from a Done JQL window (assignee/reporter + statusCategory=Done + updated > since).
- All unit tests pass (includes multi-page pagination drain test). Needs-David validation with a real Jira Cloud account.

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Jira (Cloud) (L2578–L2584). Feasibility 🟢 high. JQL `assignee` /
`reporter` = `currentUser()` scopes the pull to the user's own work.
Multiple task providers (TickTick, Linear, Todoist, Reminders) share the
ratified `tasks` contract — Jira exercises it with a third source.
