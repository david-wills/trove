# Harvest

- **id:** `harvest`
- **domains:** `time-entries/` (contract: **time-entries ✅ ratified** —
  reuse-bound to `time_entries::TimeEntry`, same domain as Toggl Track and
  Clockify; no schema touch)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll time entries; watermark by `updated_since` /
  `spent_date`)
- **connection:** `harvest` — TokenPaste (Personal Access Token **plus** a
  `Harvest-Account-Id`; both from id.getharvest.com/developers; no OAuth app
  registration for personal use). Not shared with other defs.
- **evidence:** official-docs — api.harvestapp.com/v2 (stable v2, documented
  `time_entries`, `client_credentials`-free PAT path)
- **effort / priority:** S / P2
- **needs:** none

## What it is

Time-tracking and invoicing tool popular with freelancers and agencies:
clock hours against clients, projects, and tasks. The data is the canonical
**user-asserted** time record — what the person says they spent time on,
distinct from observed activity trackers (RescueTime/Timing, which land in
`activity/<source>/`). Slots into the time-entries contract alongside Toggl
and Clockify.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Time entries | all plans | project, task, client, hours, notes, `spent_date`, started/ended | official docs |
| Clients / projects | all plans | reference names for joining entries | official docs |

Members can only access their **own** tracked time via the API — no team
visibility, which is exactly right for a personal vault. All fields optional
in the contract (omit-if-empty).

## Access & auth

- REST v2: `GET https://api.harvestapp.com/v2/time_entries` (paginate via the
  `next`/`prev` links in the response).
- Auth: **two** headers — `Authorization: Bearer <PAT>` **and**
  `Harvest-Account-Id: <id>`. Both come from the Harvest ID developer
  settings; a PAT alone is insufficient.
- No app registration for personal use. Standalone-clean (HTTPS). No TCC, no
  local files.

## Vault mapping

- **Raw layer:** `time-entries/harvest/raw/YYYY-MM.jsonl` — the v2
  `time_entries` objects, full fidelity, partitioned by month.
- **Contract layer:** `time-entries/harvest/…` per the (pending Phase 3)
  time-entries contract — one row per entry (start/end or duration, project,
  task, client, notes); billable/rounding/invoice flags to `extra`.
- **Dedupe:** entry id as `guid`; `updated_since` watermark cursor in
  `.trove/harvest-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/harvest.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: **two fields** — PAT + Harvest-Account-Id — with
   label/help/placeholder per the SimpleFIN affordance rule; make clear both
   are required).
2. Send both the `Authorization` and `Harvest-Account-Id` headers on every
   request.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from documented `time_entries` responses (with link-based
   pagination); parser + store + cursor tests, unique temp dirs.
5. Vault writes via `store` helpers once the time-entries contract is
   ratified; until then parked behind Needs-David (contract).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Time entries | 🧪 built | paste `<PAT>\|<account-id>` in the connect card; Sync now; confirm entry rows in `time-entries/harvest/` + hub last-data |
| Pagination | 🧪 built | with >1 page of entries, confirm the runner follows `next` links and backfills fully |

## Build notes (2026-06-16)

- **Status:** 🧪 — module built, 15 tests green, `cargo check` clean.
- **Contract:** `reuse-bound` / `time-entries`. `TimeEntry` with date-only `start`
  (`spent_date`), `hours*3600` → `duration_secs`, embedded project/client/task names
  (no secondary lookup needed — unlike Toggl). Running entries omit `duration_secs`.
- **Auth:** composite `TokenPaste` — user pastes `<PAT>|<account-id>`; both from
  id.getharvest.com/developers. Stored 0600 under `.trove/sync/harvest`.
- **Cursor:** `updated_since` (ISO 8601 UTC datetime); advanced only after a full
  drain to survive crashes safely.
- **Pagination:** page-based (`page` param + `links.next`); loop until no `next_url`.
- **Extra:** rates, invoice id/number, approval_status, is_billed, is_locked,
  budgeted, started_time/ended_time (12h strings), timer_started_at, updated_at.
- **Tags:** Harvest v2 time-entry objects have no tag field → `tags: []`.
- **No new deps added.**

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Harvest (Time Tracking) (L2610–L2616). Feasibility 🟢 high. Carried-forward
disambiguation: the time-tracking Harvest is at **harvestapp.com** — not to
be confused with **Greenhouse's "Harvest"** recruiting API; the unrelated
Greenhouse v3 migration warning does **not** affect this v2 time-tracking
API. Both the token and the `Harvest-Account-Id` header are mandatory. Shares
the time-entries contract with Toggl and Clockify — sequence one after
Harvest to exercise it with a second source.
