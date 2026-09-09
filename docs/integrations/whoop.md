# WHOOP

- **id:** `whoop`
- **domains:** `health/` (contract: **document** — the as-built per-source
  raw shape; Phase 3 writes the spec page, no redesign)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (OAuth poll, hourly-ish — data lands after the
  band syncs through the phone). Plus a one-shot Import path for the
  official CSV export ZIP as historical backfill.
- **connection:** `whoop` — OAuth (new connection; app registration at
  developer.whoop.com). Not shared with other defs.
- **evidence:** official-docs — developer.whoop.com (public API, v2 active
  in 2026, v1 webhooks removed); documented in-app Download-My-Data export
- **effort / priority:** M / P1
- **needs:** Needs-login (developer registration requires an active WHOOP
  membership + device — David doesn't have one; build proceeds from the
  documented shapes, validation needs any real WHOOP user)

## What it is

Screenless fitness/recovery band with a popular subscriber base. Its whole
product is the composite scores — strain, recovery, sleep performance —
and those are vendor-exclusive: WHOOP syncs only basic sleep/activity to
Apple Health, so the direct API is the only way this data reaches the
vault.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Physiological cycles | membership (all) | day strain, HR | official docs (activity/cycles) |
| Recovery | membership (all) | recovery score, HRV, RHR | official docs (recovery) |
| Sleep | membership (all) | sleep performance %, stages, quality | official docs (sleep) |
| Workouts | membership (all) | workout sessions | official docs (activity/workout) |
| Body measurements + profile | membership (all) | height/weight etc., basic profile | official docs |
| CSV export ZIP (backfill) | all | workouts.csv, sleeps.csv, journal_entries.csv — recovery/steps/VO2Max **not** in export (early 2026) | official in-app export |

All optional in the vault shape; the export-only backfill simply lacks
recovery rows until OAuth fills them.

## Access & auth

- OAuth 2.0; self-service app registration at developer.whoop.com (free,
  but requires an active WHOOP membership + device to register).
- Endpoints per the research doc: cycles, recovery, sleep, workout, body
  measurement, profile. Webhook support exists — ignore it; polling fits
  the Periodic runner. Rate limits not publicly documented — back off
  politely.
- Export: app → Profile → Privacy → Download My Data → emailed ZIP.
- No TCC, no local files. Standalone-clean: registration needs the phone
  app once, but the issued credentials pull over plain HTTPS.

## Vault mapping

- **Raw layer:** `health/whoop/<collection>.jsonl` (cycles, recovery,
  sleep, workouts, body), Oura-style keyed upserts (scores recalculate);
  month-partition anything high-volume. CSV-export rows land in the same
  files via the same keys so backfill + OAuth dedupe cleanly.
- **Contract layer:** none — `health/` is a document-domain; per-source
  raw is the shape.
- **Dedupe:** API record ids as `guid`; cursor in
  `.trove/whoop-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/whoop.rs`: `DEF` (Periodic) + import hook
   for the export ZIP; `CONNECTION` (OAuth) in `sync/whoop.rs` modeled on
   `sync/oura.rs` (token store + refresh).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from developer.whoop.com documented response shapes (v2) +
   hand-built CSV fixtures matching the documented export columns;
   parser/store/cursor tests in unique temp dirs.
4. Credential question (Needs-David adjacent): baked app credentials
   require a registered WHOOP developer account, which requires a
   membership — likely ships BYO-credentials first per the ConnectSpec
   baked+BYO model.

## As-built notes (2026-06-16)

- Module: `crates/trove-core/src/whoop.rs` + `crates/trove-core/src/sync/whoop.rs`
- Connection: new `whoop` OAuth connection (port 38669), added to CONNECTIONS registry
- Behavior: `Periodic` (hourly, advance-on-run) + manual pull hook
- Raw vault: `health/whoop/{cycles,recovery,sleep,workouts}.jsonl` + `basic.json` + `body.json` + `index.md`
- Cursor: `.trove/whoop-sync.json` — advanced only after full window drain
- CSV import functions (`run_import`, `import_workouts_csv`, `import_sleeps_csv`) are implemented and tested but not wired as a hub `Behavior::Import` box (architecture: one behavior per DEF; would need a companion import-only DEF to expose in hub)
- v2 API endpoints confirmed: `/v2/cycle`, `/v2/recovery`, `/v2/activity/sleep`, `/v2/activity/workout`, `/v2/user/profile/basic`, `/v2/user/measurement/body`
- Cycle ids are integers; workout/sleep ids are UUIDs (strings) — both coerced to string keys
- WHOOP uses rotating refresh tokens (single-use) — file-locked refresh identical to Oura pattern
- 23 tests passing (merge, upsert, import CSV, ZIP dispatch, stub HTTP server, DEF smoke)

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| OAuth pull (cycles/recovery/sleep/workouts) | 🧪 built | a real WHOOP user connects; Sync now; confirm `health/whoop/` rows + hub last-data (David has no device — any real user validates) |
| CSV export backfill | 🧪 built | call `run_import` with a Download-My-Data ZIP; confirm rows dedupe against OAuth-pulled ones (hub import box needs companion DEF) |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §WHOOP
(L838–L844). Feasibility 🟢 high. v1 webhooks were removed — build against
v2 and verify current endpoint paths in the Phase 4 loop (the research doc
lists `/v1/...` paths alongside "v2 active"; reconcile against live docs at
build time). The membership requirement is a hardware/subscription
dependency, not a software one.
