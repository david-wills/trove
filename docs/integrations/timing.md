# Timing

- **id:** `timing`
- **domains:** `activity/timing/` (raw-only — Timing is an **observed**
  tracker, so it routes to `activity/<source>/` per the taxonomy, **not**
  `time-entries/`, which is reserved for user-asserted entries like Toggl)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (local SQLite read, copy-then-read; scripted-export
  fallback only if the DB spike fails)
- **connection:** none (local app; the paid Web API path, if ever used,
  would be a localhost token, not a Trove connection)
- **evidence:** community-schema — local SQLite at
  `~/Library/Application Support/info.eurocomp.Timing2/SQLite.db`, tables
  documented by the `timingapp` Ruby gem (medium confidence); official docs
  cover only the AppleScript/JXA automation and the subscriber-gated Web
  API. **Sample-required** to confirm the gem's schema against a current
  Timing version.
- **effort / priority:** M / P2
- **needs:** Needs-sample (verify current DB schema) · Needs-login
  (validation needs a machine with Timing installed/tracking; the
  AppleScript/Web-API fallbacks additionally need a paid plan)

## What it is

Timing is a subscription macOS automatic time tracker (~$10/mo) popular
with freelancers: it observes app, document, and URL usage and rolls it
into projects. Richer context than raw app spans (it tracks *which
document* inside the app). Like Qbserve, the forward-looking overlap with
Trove's own watcher means the prize is **backfilling an existing
subscriber's multi-year history** — plus its project categorization, which
the watcher doesn't do.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| App-usage spans | any plan (local DB) | app, start/end, duration | community schema (timingapp gem) |
| Document/URL context | any plan (local DB) | document path / URL per span | community schema |
| Project categorization | any plan (local DB) | project assignment per span | community schema |
| Manual time entries | any plan (local DB) | user-created entries | community schema; routed with the rest of the raw rows |
| Scripted report export (JSON/CSV) | AppleScript: any plan with Timing running; Web API (localhost:10002): Expert/Timing Connect only | aggregated reports | official docs |

## Access & auth

- **Primary (spike): local SQLite** at
  `~/Library/Application Support/info.eurocomp.Timing2/SQLite.db` —
  copy-then-read; full activity + time-entry tables; avoids any
  subscription gating beyond owning the app. May sit under a sandboxed
  container path on some installs — research flags FDA as possible;
  permission hook should check readability and surface the FDA ask only if
  the path demands it.
- **Fallbacks (deprioritized):** AppleScript `tell application
  "TimingHelper" … save report` (exports any date range to JSON, but
  requires Timing to be running — fine as a user-triggered import, wrong
  shape for a collector) and the localhost Web API (Expert plan only).
  Both are subscriber-/runtime-gated; build them only if the DB schema
  proves illegible.
- Standalone-clean: the DB read needs nothing running; absent folder =
  no data, card explains it's for Timing users.

## Vault mapping

- **Raw layer:** `activity/timing/YYYY-MM.jsonl` — one row per observed
  span (ts, duration_secs, app, document/path, url, project), full
  fidelity. The research entries' `developer/timing/` and `time-entries/`
  framings predate the taxonomy; the path above governs.
- **Contract layer:** none — `activity/` source subfolders are raw-only.
- **Dedupe:** span primary key from the DB once confirmed; idempotent
  re-imports.

## Build plan

1. **Spike first, parser-last (Needs-sample):** on a machine with Timing,
   dump `.schema` from `SQLite.db`, reconcile against the `timingapp` gem's
   documented tables, capture a sanitized fixture. Confirm whether FDA is
   needed for the path.
2. Module `crates/trove-core/src/timing.rs`: `DEF` (Periodic, LocalSync;
   permission hook = path readability), copy-then-read.
3. One registration line in `INTEGRATIONS`. No connection.
4. Fixtures + parser/store/idempotency tests, unique temp dirs.
5. If the DB spike fails: fall back to an Import def consuming the app's
   own JSON report export (documented, official) rather than chasing the
   gated Web API.

## Schema (confirmed via marcoroth/timingapp-ruby gem)

- `AppActivity(id, startDate REAL, endDate REAL, applicationID, titleID, pathID, projectID, isDeleted)` — startDate/endDate are Unix epoch seconds as SQLite REAL (confirmed by `Time.at(value)` in gem's `time_column` helper)
- `Application(id, bundleIdentifier, title)` — app bundle + display name
- `Title(id, stringValue)` — document/window title string
- `Path(id, stringValue)` — file path or URL string
- `Project(id, title, parentID, productivityScore REAL)` — hierarchical project categories
- `TaskActivity(id, startDate REAL, endDate REAL, projectID, isRunning, isDeleted, property_bag TEXT)` — manual timer entries; `property_bag` is JSON with optional `notes` field
- DB path confirmed: `~/Library/Application Support/info.eurocomp.Timing2/SQLite.db`

## Build notes (2026-06-21)

- Replaced NotWired stub with full Periodic collector
- Reads both `AppActivity` (automatic observations) and `TaskActivity` (manual timers) — `kind` field distinguishes them in the JSONL
- Two independent rowid watermarks (`last_app_rowid`, `last_task_rowid`) in `.trove/timing-sync.json`
- Project hierarchy resolved one level up (project + project_parent fields)
- Copy-then-read via `import_via_copy` (same pattern as qbserve/iMessage/browser history) avoids holding a lock on the live DB
- Running tasks (`isRunning=1`) and soft-deleted rows (`isDeleted=1`) are filtered out
- Raw-only — `activity/timing/YYYY-MM.jsonl`; no contract bind (activity/ is raw-only domain)
- 10 unit tests pass; cargo check clean

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Spans + projects from local DB | 🧪 built | on a Mac with Timing tracking: enable, Sync now; compare a day's rows in `activity/timing/` against Timing's own report for that day; re-sync = no dupes |
| FDA prompt path | — | confirm whether first read triggers a TCC ask; card must show the affordance hint if gated |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Timing App
(L1372–L1378) and "Calendar, Tasks, Habits & Productivity" §Timing
(L2666–L2672). Both 🟡 medium. The two entries disagree on approach (M6
AppleScript agent vs. M3 local DB); the productivity-catalog entry's local
SQLite + community gem schema is the better Trove fit (no subscription
gate, no running-app dependency) and is what this brief commits to.
Subscription-based and niche → P2; only valuable to existing subscribers.
