# Things 3

- **id:** `things`
- **domains:** `tasks/` (contract: **tasks** — ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the local SQLite DB; watermark on max
  modification stamp)
- **connection:** none (local file read; no login — gated only by Full Disk
  Access)
- **evidence:** community-schema — `things.py` / `things.sh` libraries
  document the `main.sqlite` schema (`ZTASK`/`ZAREA`/`ZPROJECT`/
  `ZCHECKLISTITEM`/`ZLOGITEM`); high confidence (mature, widely used). No
  official export API.
- **effort / priority:** S / P1
- **needs:** none (FDA prompt handled by the runner; no privacy flag — tasks
  are not message-body sensitive)

## What it is

Things 3 (Cultured Code) is the highest-value local-first task manager on
the Mac: a polished GTD app used heavily by individuals. All data lives in a
local SQLite database — there is no cloud API to pull from, which makes it a
clean standalone read. Capturing it gives Trove the user's full task graph
(areas, projects, todos, checklists, completion history) with no network
calls.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Tasks (todos) | all | title, notes, status, due/start date, completion date, tags | community schema |
| Projects / areas | all | hierarchy, headings | community schema |
| Checklist items | all | per-task subitems | community schema |
| Completion log | all | logged/done timestamps (`ZLOGITEM`) | community schema |

All optional in the contract (omit-if-empty); a user with no projects simply
carries no project rows.

## Access & auth

- **Local SQLite:** `~/Library/Group Containers/JLMPQHK86H.com.culturedcode.ThingsMac/ThingsData-*/Things Database.thingsdatabase/main.sqlite`
  — standard `sqlite3` read. Beta builds use the
  `com.culturedcode.ThingsMac.beta` container path (handle both).
- **TCC:** Full Disk Access required — the DB is in a sandboxed Group
  Container another process can only read with FDA. Surface the FDA grant as
  the connect affordance (greyed toggle + inline hint until granted).
- **Safety:** reads are safe while Things runs; *writes* require the app
  quit — Trove only reads, so this is a non-issue. Open the DB read-only
  (immutable URI / `mode=ro`) to be safe against locks.
- **Standalone-clean:** no network, no running-app dependency. The Things URL
  scheme is write-only and cannot pull data — SQLite is the only route.

## Vault mapping

- **Raw layer:** `tasks/things/raw/` — periodic snapshots of the relevant
  rows at full fidelity (the native `Z*` column shapes).
- **Contract layer:** `tasks/things/` per the ratified **tasks** contract:
  one row per task (`ts`, `source`, `guid` = Things task uuid, `title`,
  `status`, `due`, `completed_at`, `project`/`area`, `tags[]`), checklist
  items and Things-specific fields (heading, deadline vs. scheduled) in
  `extra`.
- **Dedupe:** Things task uuid as `guid`; cursor in
  `.trove/things-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/things.rs`: `DEF` (Periodic), local-file
   permission hook keyed on FDA + container glob, `pull` for Sync-now.
2. Register one line in `INTEGRATIONS` (no `CONNECTIONS` entry — no login).
3. Resolve the `ThingsData-*` glob and beta path; open read-only.
4. Fixtures: a small fixture `main.sqlite` covering todo/project/area/
   checklist/logged shapes; parser + store + cursor tests, unique temp dirs.
5. Map Things date encoding (its float/day-number fields) carefully to the
   contract's ISO timestamps — note this in tests.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Tasks + projects | ✅ built | grant FDA; Sync now; confirm rows in `tasks/things/` + hub last-data against the live app |
| Completion log | ✅ built | complete a task in Things, re-sync, confirm `completed_at` appears in events/ JSONL |

## Build notes (2026-06-16)

**Date encoding confirmed from real DB:** `creationDate`/`userModificationDate`/`stopDate`
are Unix float timestamps (seconds since 1970). `startDate` and `deadline` use a
bit-packed integer: `year<<16 | month<<12 | day<<7` — verified against 5 real rows
from the live DB (e.g. 132743040 = 2025-07-31). Decoder in `things_date_to_iso`.

**Schema confirmed:** Tables are `TMTask`, `TMArea`, `TMChecklistItem`, `TMTag`,
`TMTaskTag` — NOT the ZPrefix Core Data names mentioned in the brief (the community
schema docs use "Z" prefixes for Bear/Apple apps but Things uses "TM" prefixes).
Brief corrected accordingly.

**Behavior:** Periodic 15-min poller. Copy-then-open via `import_via_copy` (never
locks the live DB). Fate resolution from the TMTask set itself (status=3 → Completed
with stopDate; trashed=1 → Deleted; otherwise Unknown carry-forward).

**Not narrowed from brief:** checklist items, tags, both raw and contract layers all
shipped.

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Things 3 (L2498–L2504). Feasibility 🟢 high. Gotchas: glob the
`ThingsData-*` dir, handle the beta container path, open read-only, and
translate Things' date encoding. `things.py`/`things.sh` are the reference
schemas. Sequence after Reminders/TickTick so the tasks contract is already
exercised by multiple sources.
