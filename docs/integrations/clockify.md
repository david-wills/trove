# Clockify

- **id:** `clockify`
- **domains:** `time-entries/` (contract: **Phase 3 pending** — time-tracking
  shape, drafted with Toggl Track; **user-asserted entries only**, distinct
  from observed trackers which land in `activity/<source>/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the time-entries endpoint; watermark cursor on
  entry start time)
- **connection:** `clockify` — TokenPaste (API key from Clockify Profile
  Settings > API; `X-Api-Key` header; no OAuth). Not shared with other defs.
- **evidence:** official-docs — api.clockify.me/api/v1 (`/user`,
  `/workspaces/{id}/user/{id}/time-entries`, documented 10 req/s limit,
  pagination)
- **effort / priority:** S / P2
- **needs:** none — time-entries contract already ratified by toggl_track.rs (pioneer build)

## What it is

Time-tracking tool: the user manually starts/stops timers or logs entries
against projects and tasks. Very popular with freelancers and teams; the
free tier has full API access. The data is a *user-asserted* record of how
time was spent (project, task, description, duration) — an intentional log,
not a passive observation, which is why it routes to `time-entries/` and
never to the observed-activity stream.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Time entries | all plans incl. free | start/end, duration, project, task, tags, description, billable | official docs |
| User/workspace identity | all plans | userId, workspaceId (to address the entries endpoint) | official docs |
| Reports (aggregated) | all plans | rollups by project/tag | official docs (not needed — raw entries suffice) |

All optional in the contract; tiering needs no special code paths.

## Access & auth

- REST: `GET /user` returns current userId + workspaceId, then
  `GET /workspaces/{workspaceId}/user/{userId}/time-entries` (paginated via
  page / page-size). `X-Api-Key` header (user-level — only the user's own
  entries).
- Rate limit: 10 req/s — trivially fine for a periodic personal pull.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `time-entries/clockify/raw/YYYY-MM.jsonl` — the API entry
  objects, full fidelity.
- **Contract layer:** `time-entries/clockify/YYYY-MM.jsonl` per the (pending)
  time-entries contract — expected shape: one row per entry (`ts` = start,
  `source`, `guid` = entry id, `end`, `duration_secs`, `project`, `task`,
  `description`, `tags[]`), billable flag + workspace in `extra`.
- **Dedupe:** entry id as `guid`; cursor in `.trove/clockify-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/clockify.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: label/help/placeholder per the SimpleFIN
   affordance rule), `pull` hook that first resolves userId/workspaceId via
   `/user`, then pages the entries endpoint.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`; `connection:
   Some("clockify")`.
3. Fixtures from api.clockify.me example responses (entries with/without
   project + task); parser + store + cursor tests, unique temp dirs.
4. Vault writes via `store` helpers once the time-entries contract is
   ratified; until then **parked behind Needs-David (contract)**. Sequence
   alongside Toggl Track so the contract is exercised by two sources.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Time entries | ✅ built | paste a real API key in the connect card; Sync now; confirm rows in `time-entries/clockify/` + hub last-data; works on free plan |
| Identity resolution | ✅ built | confirm `/user` resolves userId/workspaceId without the user pasting IDs |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Clockify (L2602–L2608). Feasibility 🟢 high. Free tier has full,
unthrottled API access. Pairs with Toggl Track on the same time-entries
contract — sequence one right after the other to ratify the shape with two
sources. User-asserted only: never conflate with observed trackers
(RescueTime, Timing) which land in `activity/<source>/`.
