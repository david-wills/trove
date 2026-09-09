# Toggl Track

- **id:** `toggl-track`
- **domains:** `time-entries/` (contract: **ratified** — `TimeEntry` bound,
  registered in `DOMAINS`; **first collector in the `time-entries` domain**,
  user-asserted entries only)
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-login**
- **unavailable_reason:** none
- **behavior:** Periodic (hourly; poll for new/changed entries; `since`
  watermark cursor)
- **connection:** `toggl-track` — TokenPaste (a personal **API token** copied
  from the bottom of **track.toggl.com/profile**; no OAuth, no
  bring-your-own-app step — self-service). HTTP Basic with the token as the
  username and the literal `api_token` as the password (Toggl's documented
  scheme); verified at connect with a real `GET /api/v9/me`; stored 0600 under
  `.trove/sync/`, never logged or written to the cursor. Timery (`timery`) is a
  Toggl frontend with no independent store, so it is covered by this provider —
  document, don't build separately.
- **evidence:** official-docs — api.track.toggl.com API v9 (shipped path) +
  community-schema — local CoreData SQLite (`ZMANAGEDTIMEENTRY`,
  reverse-engineered, medium-high confidence; **not** built this pass — see
  Build plan)
- **effort / priority:** S / P1
- **needs:** **Needs-login** — live validation needs a real Toggl Track API
  token (no app registration). The contract is now ratified, so no Needs-David
  (contract) block remains.

## What it is

One of the most popular manual time trackers: the user starts/stops timers or
logs entries against projects, tasks, tags, and clients. Distinct from
observed trackers (RescueTime, Timing) — these are **user-asserted** entries,
which is why they route to `time-entries/` rather than `activity/`. High-value
for answering "how did I actually spend my hours" by the user's own account.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Time entries | all plans | start, stop, duration, description, project FK, tags | community DB + official API |
| Projects / clients / tags | all plans | names, colors, client FK | official API |
| Running timer | all plans | current entry | official API |

All optional in the (pending) time-entries contract; omit-if-empty.

## Access & auth

- **Local DB (preferred, fast):** CoreData SQLite at `~/Library/Group
  Containers/B227VTMZ94.group.com.toggl.daneel.extensions/production/DatabaseModel.sqlite`
  — 19 entity tables; `ZMANAGEDTIMEENTRY` holds start/stop/description/project
  FK. Needs Full Disk Access (TCC) to read the Group Container. Read-only;
  open with `immutable=1` while the app may be running.
- **API v9 (cross-device history):** `GET
  https://api.track.toggl.com/api/v9/me/time_entries`; API token as Basic-Auth
  password. Rate limit **30 req/hour** for `/me` endpoints — page carefully,
  watermark hard.
- Standalone-clean: local SQLite + plain HTTPS; no runtime app dependency.

## Vault mapping

- **Raw layer:** `time-entries/toggl-track/raw/YYYY-MM.jsonl` — verbatim v9
  API entry objects, full fidelity. Deduped by `id`+`at` (last-modified), so a
  running→stopped re-emit is recorded as a second snapshot while an unchanged
  re-poll collapses.
- **Contract layer:** `time-entries/toggl-track/YYYY-MM.jsonl` per the
  **ratified** time-entries contract — one `TimeEntry` row per entry:
  `source`, `id` (the entry id, the dedupe key), `start` (UTC→**local**, the
  month partition key), `end` + `duration_secs` (both omitted while a timer
  runs), `description`, `project` (resolved from `/me/projects` id→name),
  `tags[]`, `billable`. The contract `client` is left blank (the v9 entry
  carries no client; resolving the client *name* needs a separate `/me/clients`
  call we skip under the 30/hr cap — the client *id* is preserved in `extra`);
  `task` is an id, kept in `extra`. Source-specific bits
  (`workspace_id`/`project_id`/`user_id`/`task_id`/`tag_ids`/`duronly`/`at`)
  ride in `extra`. Append-only: the first observed state of an `id` wins, so an
  entry first synced mid-timer keeps its open row and its final duration lands
  only in `raw/`.
- **Dedupe:** entry `id` (contract, first-wins) / `id`+`at` (raw); `since`
  watermark cursor in `.trove/toggl-track-sync.json` (non-secret, rebuildable by
  scanning output files — carries no token).

## Build plan

**Shipped this pass (API v9 only):**

1. ✅ Module `crates/trove-core/src/toggl_track.rs`: `DEF` (Periodic, hourly),
   `CONNECTION` (TokenPaste), `pull` hook for Sync-now. Injectable `TogglApi`
   trait so the mapping/persist logic is fixture-tested fully offline.
2. ✅ `CONNECTIONS` registration line (the `INTEGRATIONS` line already existed
   from the Phase-2 stub).
3. ✅ `time-entries` contract **ratified**: `TimeEntry` Rust type + `DOMAINS`
   entry + `time_entries` module; promoted out of the Phase-3 draft array,
   round-trip `check::<TimeEntry>` + doc-sync + schema-required rows added.
4. ✅ Two-endpoint API pull (`/me/projects` for id→name + id→client_id,
   `/me/time_entries`) with a `since` watermark; raw + contract layers; 401/429
   handled with clear messages; soft-deleted tombstones move the watermark but
   aren't written as active rows. 19 unit tests + the spec round-trip.

**Deferred (clean follow-up slices, same module):**

5. **Local CoreData SQLite path** (`ZMANAGEDTIMEENTRY` under the Group
   Container, FDA-gated, no network — the offline/fast primary). NOT built this
   pass: the API path is self-contained and proves the contract end-to-end. A
   later slice can add dual-path resolution (prefer the local DB when present +
   FDA granted, else the rate-limited API) and must watch the macOS-upgrade
   DB-lock regression (fixed in Toggl v10.15.0) — degrade to the API on open
   failure. Logged in the journal.
6. **Deep backfill** beyond Toggl's default recent window (date-range paging
   against the 30 req/hr `/me` cap). The lean incremental pull is the default.

## Validation matrix

Promotion to ✅ needs David's real Toggl Track API token (Needs-login). No app
registration — the token is self-service from the profile page.

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connect + auth | 🧪 fixture | `connection_exposes_token_paste_method`, `connection_stores_token_0600_and_absent_from_cursor`, `empty_token_rejected_and_pull_needs_connection`, `auth_header_is_basic_token_colon_api_token`. **David:** open **track.toggl.com/profile** (signed in) → scroll to the bottom → copy your **API token** → paste it into the **Toggl Track** connect card → it verifies with a real `GET /api/v9/me` (HTTP Basic, `token:api_token`) and stores 0600 at `.trove/sync/toggl-track`. A 401 says to recopy the token from the profile page. |
| Time-entry pull | 🧪 fixture | `full_pull_writes_both_layers_skips_tombstone_and_advances_watermark`, `maps_stopped_entry_with_project_name_tags_billable_and_local_times`, `running_timer_omits_end_and_duration`, `deleted_tombstone_is_detected`. **David:** enable the **Toggl Track** toggle (default-off) → **Sync now** → confirm `TimeEntry` rows in `time-entries/toggl-track/YYYY-MM.jsonl` (each with `source`/`id`/`start`; stopped entries also `end` + `duration_secs`; `project` = the resolved project **name**; `tags`/`billable` where set) + the lossless `time-entries/toggl-track/raw/` mirror + the hub "last data" date. A running timer shows as a row with `start` and **no** `end`/`duration_secs`. |
| Incremental cursor (`since`) | 🧪 fixture | the re-run half of `full_pull_writes_both_layers_skips_tombstone_and_advances_watermark` (second pull sends the stored `since`, writes 0 new contract rows, byte-identical file), `cursor_back_compat_empty_and_partial_deserialize`, `fetch_error_does_not_advance_watermark`. **David:** log a new entry in Toggl → **Sync now** again → exactly one new row appears; the first sync backfilled the recent window, later syncs only fetch what changed since the watermark. |
| Running→stopped (append-only) | 🧪 fixture | `running_then_stopped_keeps_open_contract_row_but_raw_gets_both_snapshots`. **David:** start a timer → **Sync now** (open row, no duration) → stop the timer → **Sync now** → the contract row stays the first-observed **open** state (by design), while the final `end`/`duration` are preserved in `time-entries/toggl-track/raw/` (two snapshots). A reader wanting the closed figure reads `raw/`. |
| Project-name + client linkage | 🧪 fixture | `project_id_name_needs_both`, `project_id_client_only_when_client_present`, the `extra.client_id` assertion in `maps_stopped_entry_...`. **David:** confirm an entry's `project` is the human project **name** (not the id); a since-deleted project leaves `project` blank. The client **name** is intentionally blank (no `/me/clients` call under the rate cap) but the client **id** is preserved under `extra.client_id`. |
| Secret hygiene | 🧪 fixture | `connection_stores_token_0600_and_absent_from_cursor` (asserts 0600 mode + token absent from the cursor), the cursor assertion in `full_pull_writes_both_layers_...` (token never in `.trove/toggl-track-sync.json`). **David:** none — automated. |
| Local CoreData SQLite path | deferred | not built this pass — the API read path proves the contract end-to-end. The FDA-gated, offline local-DB path (`ZMANAGEDTIMEENTRY`) is a clean follow-up slice in the same module (logged in the journal). |
| Deep history backfill | deferred | first sync takes Toggl's default recent window (~last few months); older history isn't deep-backfilled (date-range paging against the 30 req/hr `/me` cap is a follow-up). |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity" §Toggl
Track (L2522–L2528). Feasibility 🟢 high. The 30 req/hour `/me` API cap makes
the local DB the primary path. macOS 26 introduced a transient DB-open failure
after OS upgrade (Toggl fixed in v10.15.0) — monitor for regressions and fall
back to the API. Timery (`timery`) is entirely covered by this provider —
document, don't build separately.
