# Habitica

- **id:** `habitica`
- **domains:** `habits/` — **first collector to bind the `habits/` contract**
  (INDEX #96): adds `crate::habits::{Habit, Checkin}`, the `habits` `DOMAINS`
  entry (`SnapshotPlusEvents`), and promotes the `habits.habit` + `habits.checkin`
  Phase-3 drafts to ratified in `spec_validation`. (Streaks / Habitify /
  TickTick-habits / Way-of-Life later reuse this same contract.)
- **scope note:** this collector ships **habits + dailies only** (definitions →
  the snapshot, per-day history → check-ins). Habitica's **to-dos and rewards
  are out of scope** — they are not habits; a separate `tasks/`-domain slice can
  add them later. The original dual-domain (`habits/` + `tasks/`) brief was
  narrowed to ship the habits domain cleanly.
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-login**
- **unavailable_reason:** none
- **behavior:** `Behavior::Periodic` — hourly (`HABITICA_SYNC_SECS = 3600`),
  every-on-run cadence (the timer only advances when it actually runs, so
  re-enabling fires immediately). Each pass `GET`s `/api/v3/tasks/user?type=habits`
  and `?type=dailys`, rewrites the habit-definition snapshot whole, and appends
  only history points newer than the stored `updatedAt`/last-date watermark.
- **connection:** `habitica` — TokenPaste (User ID + API Token from Habitica
  **Settings → Site Data → API**; no app registration, no OAuth). Pasted as
  `USER_ID:API_TOKEN`, verified at connect with a real `GET /api/v3/user` (a 401
  bails with a reconnect message), stored 0600 at `.trove/sync/habitica` (token
  in `access_token`, the non-secret User ID in `scope`). **A new inline
  connection — not shared with other defs.**
- **default:** off (`default_on: false`) — a Needs-login cloud sync, off until
  the user pastes a credential; the def is toggleable once connected.
- **evidence:** official-docs — habitica.com/api/v3 (v3 is the only supported
  version; documented `/user`, `/tasks/user`, and bulk export endpoints)
- **effort / priority:** S / P2
- **needs:** **Needs-login** (real-data validation needs a Habitica account);
  time-sensitive (Habitica averages/discards older task history — **connect
  early** to capture detail before it's coarsened)

## What it is

Gamified habit-and-task tracker: habits, dailies, and to-dos earn XP, gold,
and streaks in an RPG frame. Used by people who need game mechanics to stick
with routines. The data is uniquely shaped — habit checkins + streaks + an
RPG progression log. **As shipped, this collector takes the habit/daily side
only:** each habit and daily definition becomes a `Habit` snapshot record, and
each per-day history point becomes a `Checkin` event. The RPG progression
(XP/level/gold/class), streak counts, reminders, and per-day raw blob ride in
`extra` / the raw layer at full fidelity. To-dos are left for a future
`tasks/`-domain slice.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Habits + dailies | free | title, streak, counterUp/Down, history | official docs |
| To-dos | free | title, due, completed, checklist | official docs |
| RPG progression | free | XP, level, gold, class | official docs |
| Full bulk export | free | entire user object as JSON | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- REST v3: `GET /api/v3/user` (full user data incl. XP/streaks/history),
  `GET /api/v3/tasks/user?type=habits|dailys|todos`; bulk export
  `GET /api/v3/user/export/userdata.json`.
- Auth headers: `x-api-user` (User ID) + `x-api-key` (API Token) +
  **`x-client`** (mandatory since late 2025 — must be a unique app
  identifier; set it to a stable Trove string).
- No app registration; keys are free, found in-app. Standalone-clean (HTTPS).
- No TCC, no local files.

## Vault mapping

- **Raw layer:** `habits/habitica/raw/YYYY-MM.jsonl` — the API task objects,
  full fidelity (the per-type `/tasks/user` lists, each with its inline
  `history` blob).
- **Contract layer (`habits/` — the contract this collector ratifies):**
  - **Snapshot:** `habits/habitica/habits.jsonl` — one `Habit` row per habit /
    daily, rewritten whole each sync (`source`, `id`, `title` required; cadence
    in `schedule`, measurable target in `goal`/`unit`; streak counts, RPG
    XP/level/gold, reminders, icon → `extra`).
  - **Events:** `habits/habitica/checkins/YYYY-MM.jsonl` — one append-only
    `Checkin` row per habit-day (`date`, `source`, `habit`, `status` required;
    `status` normalized to `done`/`skipped`/`missed`, never guessed from a
    partial value; logged amount → `value`; goal-at-the-time / raw stamp →
    `extra`).
- **Dedupe:** habit `id` keys the snapshot; check-ins dedupe on
  `source`+`habit`+`date` (one row per habit per day). Watermark cursor in
  `.trove/habitica-sync.json` (non-secret, rebuildable by scanning output).

## Build plan (as shipped)

1. ✅ New `crate::habits` module with `Habit` (snapshot) + `Checkin` (event)
   record shapes; `habits` `DOMAINS` entry (`SnapshotPlusEvents`); `habits.habit`
   + `habits.checkin` Phase-3 drafts promoted to ratified in `spec_validation`.
2. ✅ Module `crates/trove-core/src/habitica.rs`: `DEF` (`Behavior::Periodic`,
   hourly), `CONNECTION` (TokenPaste — `USER_ID:API_TOKEN`, with
   label/help/placeholder per the SimpleFIN affordance rule), `pull` + `collect`
   hooks. Injectable HTTP trait so all 18 tests run fully offline.
3. ✅ The mandatory `x-client` header (`trove-habitica`) on every request —
   Habitica rejects requests without it since late 2025.
4. ✅ Registration: `&habitica::DEF` (already present as the Phase-2 stub, now
   fleshed out) in `INTEGRATIONS`; `&habitica::CONNECTION` added to `CONNECTIONS`.
5. ✅ Fixtures from the documented `/tasks/user` shapes; parser maps habit/daily
   definitions → snapshot, each `history` point → a check-in (status normalized,
   never guessed from a partial value); snapshot+checkins+raw writes, `updatedAt`
   watermark, dedupe — all tested against unique temp dirs.
6. ⏭️ To-dos slice (`tasks/` domain) deferred — a clean same-module follow-up.

## Validation matrix

Promotion to ✅ needs David's real Habitica credential (Needs-login). No app
registration is required — the User ID and API Token are self-service, found
in-app. All 18 module tests pass against fixtures (offline, injected HTTP).

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connect + auth | 🧪 fixture | `connection_exposes_token_paste_method`, `split_credential_parses_user_and_token`, `empty_login_rejected_and_pull_needs_connection`. **David:** in Habitica open **Settings → Site Data → API** (signed in) → copy your **User ID** and **API Token** → paste them into the **Habitica** connect card **joined by a colon** as `USER_ID:API_TOKEN` → it verifies with a real `GET /api/v3/user` (a 401 returns a "copy fresh" message) and stores the credential 0600 at `.trove/sync/habitica`. |
| Habit / daily snapshot | 🧪 fixture | `parse_tasks_reads_data_envelope_and_bare_array`, `maps_habit_definition_with_provenance_in_extra`, `maps_daily_repeat_map_to_weekday_schedule`, `repeat_schedule_is_week_ordered`. **David:** enable the **Habitica** toggle (default-off) → **Sync now** → confirm one `Habit` row per habit/daily in **`habits/habitica/habits.jsonl`** (`source`=`habitica`, `id`, `title`; daily cadence in `schedule`; streak/XP/level/gold/reminders in `extra`) + the hub "last data" date. Re-sync → the snapshot is rewritten whole (no duplicate rows). |
| Check-in history | 🧪 fixture | `habit_history_point_maps_to_done_checkin_with_local_date`, `habit_minus_press_is_missed`, `compressed_habit_point_is_skipped_not_guessed`, `daily_history_status_vocabulary`, `checkin_skips_point_without_usable_date`. **David:** after a sync, confirm `Checkin` rows in **`habits/habitica/checkins/YYYY-MM.jsonl`** — one per habit-day (`date`, `habit`=the habit id, `status` ∈ `done`/`skipped`/`missed`; a measurable habit carries `value`). Mark a habit done in Habitica today → **Sync now** → a `done` check-in for today's date appears. |
| Incremental + watermark | 🧪 fixture | `full_pull_writes_snapshot_checkins_raw_dedupes_and_advances_watermark`, `incremental_pull_only_appends_newer_history`, `pull_drops_preened_history_but_keeps_it_raw_and_advances_watermark`, `fetch_failure_aborts_without_advancing_watermark`, `cursor_back_compat_empty_and_unknown_fields`. **David:** **Sync now** twice with no new activity → the second pass adds **no** new check-in rows (watermark in `.trove/habitica-sync.json` held). The lossless raw mirror lands under `habits/habitica/raw/`. |
| Secret hygiene | 🧪 fixture | `connection_stores_credential_0600_and_absent_from_cursor` (asserts 0600 on the stored token **and** that neither the token nor the User ID lands in the non-secret cursor). **David:** none — automated. |
| To-dos (`tasks/` domain) | deferred | not built in this pass — habits + dailies only. To-dos and rewards are a clean same-module `tasks/`-domain follow-up (logged in the journal). |
| Bulk export | deferred | the per-type `/tasks/user` reads cover the habit/daily history; the `/user/export/userdata.json` archive is unnecessary for this slice. |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Habitica (L2570–L2576). Feasibility 🟢 high. v3 is the **only** supported
version (v1/v2 shut down). Two carried-forward gotchas: the `x-client` header
is mandatory as of late 2025, and Habitica **averages/discards older task
history** — so the time-sensitive flag is real: connect early to capture
detailed checkin logs before they're coarsened. Streaks/TickTick-habits will
share the habits contract — sequence one after Habitica to exercise it.

**Outcome (2026-06-15):** Habitica is the **first collector to bind the
`habits/` domain** — `habits.habit` + `habits.checkin` are now ratified (Rust
types `Habit`/`Checkin`, `DOMAINS` entry, round-trip checks). Both `x-client`
and the time-sensitive history-coarsening gotchas were honored (mandatory
header set; hourly every-on-run pull). To-dos were de-scoped to keep the habits
binding clean — a follow-up `tasks/` slice can add them. The next habits-domain
collector (Streaks / Habitify / TickTick / Way-of-Life) reuses this contract
with **no** struct / `DOMAINS` / `spec_validation` change.
