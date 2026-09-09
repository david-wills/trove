# GitHub

- **id:** `github`
- **domains:** `developer/` (raw-only) + `tasks/` (contract: ✅ **ratified**
  — assigned issues are assigned-work shape per the routing rule)
- **status:** 🧪 built (fixture-tested — API pull + tasks routing; needs a PAT to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (API pull) + Import (account-archive ZIP backfill)
- **connection:** `github` — new connection; TokenPaste (fine-grained PAT)
  first, OAuth optionally later. Shared by the periodic def(s); the archive
  import needs no login.
- **evidence:** official-docs — REST API at api.github.com, documented
  endpoints and rate limits (5,000 req/hr per PAT); account-archive export
  is an official feature
- **effort / priority:** S / P0
- **needs:** Needs-login (mint a fine-grained PAT to validate the live pull);
  archive-ZIP backfill slice deferred → Needs-sample (request the account
  export, drop in `~/Trove-samples/`)

## What it is

The cloud half of developer activity: commits on repos you don't have
cloned, PRs, issues, stars, and gists. Complements `local-git` — local git
captures uncommitted/unpushed work; GitHub captures collaboration and the
parts of your history living only upstream. Near-universal among
developers; the highest-value cloud developer source.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Commits (per repo, by author) | PAT `repo` scope for private | repo, sha, ts, message, stats | official docs (`/repos/{o}/{r}/commits?author=`) |
| PRs & issues | same | title, state, ts, repo, labels, assignee | official docs |
| Assigned issues → tasks | same | task title, status, due-less, project=repo | official docs |
| Stars & gists | `read:user` | repo/gist id, ts | official docs (`/user/starred`, `/gists`) |
| Recent events feed | public events only, 300-event rolling window | event type, repo, ts | official docs (`/user/events`) |
| Full-history backfill | none (any account) | all commits/issues/PRs as JSON | official account-archive ZIP (Settings → Export account data) |

All optional; private-repo slices simply absent on a narrow-scope PAT.

## Access & auth

- REST API v2026-03-10, Bearer PAT with `read:user` + `repo` scopes.
  5,000 req/hr per PAT — iterating repos page-by-page on first run is fine
  for personal accounts.
- `/user/events` only holds the last 300 public events — never a backfill
  path. Backfill = the account-archive ZIP via the generic import box.
- Standalone-clean: plain HTTPS; TokenPaste avoids needing an OAuth
  client at all for v1 (baked OAuth creds can come later per ConnectSpec).

## Vault mapping

- **Raw layer:** `developer/github/commits/YYYY-MM.jsonl`,
  `developer/github/prs/YYYY-MM.jsonl`, `developer/github/issues/YYYY-MM.jsonl`,
  `developer/github/stars.jsonl`, `developer/github/gists.jsonl` —
  taxonomy: `developer/` is raw-only.
- **Contract layer:** issues assigned to the user additionally write
  `tasks/github/` rows per the **ratified tasks contract** (title, status
  open/done, completed ts = closed ts, project = repo, overflow in
  `extra`). The developer firehose itself joins no contract.
- **Dedupe:** `guid` = node id (commits: `<repo>:<sha>`); per-endpoint
  cursors in `.trove/github-sync.json`; archive import dedupes against
  API-pulled rows by the same guids.

## Build plan

1. Module `crates/trove-core/src/github.rs`: `DEF` (Periodic) +
   `CONNECTION` (TokenPaste; setup copy = where to mint a fine-grained PAT
   and which scopes, per the SimpleFIN affordance rule).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Pull order: repos list → per-repo commits-by-author (incremental
   `since=`), PRs/issues via search-by-involves, stars/gists paginated.
4. Archive backfill: accept the official ZIP via the registry import box
   (Letterboxd pattern); parse the JSON inside; same guids dedupe overlap.
5. Tasks routing: assigned-issue rows through the tasks `store` helper —
   write a fixture where one issue is both in the firehose and assigned,
   asserting it lands in both folders (records route whole; these are two
   record types, not one record split).
6. Fixtures from official docs example responses; rate-limit-aware
   pagination with backoff on 403/secondary-limit.

## Build status — 🧪 2026-06-14

Shipped (`github.rs`, INDEX #5): `Behavior::Periodic` (hourly) +
`ConnectMethod::TokenPaste` connection (fine-grained PAT; verified via
`GET /user`; stored 0600 via `save_sync_token`, oura/lastfm precedent).
Pull = `/user/repos` → per-repo commits-by-author (`since=`),
`/search/issues` for issues + PRs (`involves:<login>`, `is:pr` discriminates),
`/user/starred` (with the `vnd.github.star+json` media type for `starred_at`),
`/gists`. Raw layer as mapped above with typed structs
(`#[serde(flatten)] extra` = full fidelity) + synthesized guids, `Link`-header
pagination, upsert-into-partition dedup, cursor `.trove/github-sync.json`.
Assigned **open** issues route to `tasks/github/` via the bound contract's
`apply_tasks_sync` (a *later* collector in an already-bound domain — no
struct/`DOMAINS`/`spec_validation` change); `fate`: closed→`Completed(closed_at)`,
unassigned→`Deleted`, error→`Unknown`. The same assigned issue appears in BOTH
the `developer/github/issues/` firehose and `tasks/github/`.

Adversarial-verify caught + fixed: (1) stars were silently dropped without the
`Accept: application/vnd.github.star+json` header (real API returns bare repo
objects, no `starred_at`/`repo` wrapper) — now sent per-call; parser degrades
(emits a row without `starred_at`) rather than dropping; (2) commits cursor now
tracks committer date (what `since=` filters on; rows still partition by author
month); (3) `/search/issues` uses `sort=updated&order=asc` + advance-to-max-
processed to avoid the 1000-result-cap tail skip.

**Deferred slices:** commit diff `stats` (additions/deletions — only on the
single-commit GET, N extra calls); the **account-archive ZIP backfill**
(Needs-sample — migration JSON layout undocumented, can't parse blind); OAuth
connect method (TokenPaste suffices for v1). The `/search` 1000-cap and
`/user/events` 300-event window mean the archive is the only true full-history
path.

Gate: trove-core 414/0 (+20 github tests), `cargo check` clean, `schedule_doc`
regenerated, `bindings.ts` up to date.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| API pull (commits/PRs/issues/stars/gists) | 🧪 | paste a real PAT; Sync now; spot-check rows against github.com profile; hub last-data updates |
| Tasks routing | 🧪 | assign yourself an issue; Sync now; row appears in `tasks/github/` and in the tasks view |
| Archive backfill | 🚫 deferred (Needs-sample) | request the account archive ZIP, drop it in `~/Trove-samples/`; build the import DEF, then confirm pre-PAT history appears and no duplicate guids |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §GitHub
(L1316–L1322). Feasibility 🟢 high; already "planned" in data-sources.md.
Third-party precedent: ghexport (Python) covers events/repos/stars — ours
compiles in instead. `gitlab` and `bitbucket` are separate providers
following the same pattern and the same `developer/` schema family — build
GitHub first, then clone the shape.
