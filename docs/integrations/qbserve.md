# Qbserve

- **id:** `qbserve`
- **domains:** `activity/qbserve/` (raw-only — `activity/` subfolders carry
  imported observed-span histories; the root day-files stay the live
  watcher's single-writer stream)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (local SQLite read; copy-then-read the daily
  `Backup.sqlite` to dodge the app's write lock)
- **connection:** none (fully local app, no account)
- **evidence:** schema confirmed via adamfortuna/qbserve_to_exist (SQL: `log.activity_id →
  Activities._id`, `a.category_id → Categories._id`) and Avery2/Qbserve-Blocker R Markdown
  (Activities has `_id`,`title`; Apps has `_id`,`localized_name`; join is separate). Key
  tables: `z_HistoryLog_YYYY_M` (activity_id→Activities._id, start_time=Unix epoch secs,
  duration=secs), `Activities` (title, category_id, app_id?), `Apps` (bundle, localized_name;
  optional via Activities.app_id), `Categories` (productivity int: 1=Productive, 0=Neutral,
  -1=Distracting), `HistoryTablesIndex` (table names). Parser shipped; 12/12 tests pass.
- **effort / priority:** M / P2
- **needs:** schema confirmed from community reverse-engineering (Avery2/Qbserve-Blocker);
  recommend validation on a real Qbserve install to confirm field completeness

## What it is

Qbserve is a one-time-purchase macOS time tracker that automatically logs
app/website usage and assigns productivity categories, entirely locally (no
cloud). It overlaps with Trove's own activity watcher going forward — its
real value is **backfill**: existing Qbserve users may have years of
categorized app-usage history sitting in a local database that Trove can
absorb once.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| App/website usage spans | none (one-time purchase) | app name, bundle id, duration, day/timestamp | path confirmed; schema undocumented |
| Productivity categories | none | app → category mapping (productive/neutral/distracting) | expected table, needs sample |
| Tracked URLs | only via Firefox/Vivaldi/Opera/Yandex extensions (not Chrome/Safari) | url spans | research notes |

All optional in the raw shape — whatever columns the schema actually has
are carried; nothing is invented.

## Access & auth

- Local SQLite at `~/Library/Application Support/Qbserve/UserDatabase.sqlite`;
  a daily backup copy sits beside it as `Backup.sqlite`. No public API; CSV/
  timesheet export exists in the app UI but its format is also undocumented.
- Read `Backup.sqlite` (or copy-then-read the live DB) — the app holds a
  write lock while running.
- No TCC prompt expected (plain home-dir Application Support path, no FDA).
  Standalone-clean: no dependency on Qbserve running; if the folder is
  absent the integration simply reports no data.

## Vault mapping

- **Raw layer:** `activity/qbserve/YYYY-MM.jsonl` — one row per observed
  span (expected: ts, duration_secs, app, bundle_id, category, url?), full
  fidelity from whatever the schema yields.
- **Contract layer:** none — `activity/` has no contract (the root stream
  is owned by the live watcher; source subfolders are raw-only per the
  taxonomy). Note: the research entry's `developer/`-era framing predates
  the taxonomy; the path above governs.
- **Dedupe:** span identity from rowid/ts+app composite once the schema is
  known; re-imports must be idempotent.

## Build plan

1. **Spike first, parser-last (Needs-sample):** on a machine with Qbserve,
   inspect `UserDatabase.sqlite` (`sqlite3 .schema`) and capture a
   sanitized sample fixture. Expected shape is simple (records: app,
   duration, day; categories: app → category) — confirm before committing
   the M effort.
2. Module `crates/trove-core/src/qbserve.rs`: `DEF` (Periodic, LocalSync;
   permission hook = path-exists check), copy-then-read of `Backup.sqlite`.
3. One registration line in `INTEGRATIONS`. No connection.
4. Fixtures from the captured sample; parser + store + idempotent-reimport
   tests, unique temp dirs.
5. Hub card should make the audience clear: "for existing Qbserve users —
   backfills your tracked history."

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Usage spans + categories | built (needs real install) | on a Mac with Qbserve history: enable, Sync now; confirm rows in `activity/qbserve/` match a day visible in Qbserve's own UI; re-sync produces no duplicates |
| Lock avoidance | built | sync while Qbserve is running; Backup.sqlite is preferred; copy-then-read used as fallback for live DB |
| Idempotent re-import | ✅ tested | 12/12 unit tests pass including idempotency and cursor advance |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Qbserve App
Time Tracker (L1364–L1370). Feasibility 🟡 medium — purely on the
undocumented schema, not on access. Niche (paid app) but a good
privacy-respecting Trove fit for its users. Main caveat: the live watcher
already captures equivalent data going forward, so position this as
historical backfill, not an ongoing collector users should seek out.
