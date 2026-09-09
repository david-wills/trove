# Oura Ring

- **id:** `oura`
- **domains:** `health/` (contract: **document** — the as-built per-source
  raw + per-metric shape; Phase 3 writes the spec page without redesigning
  it)
- **status:** 🧪 built (shipped pre-pipeline; only David promotes to ✅)
- **unavailable_reason:** none
- **behavior:** Periodic (hourly; request-budgeted inside the watcher owner
  loop, unbudgeted on manual Sync-now)
- **connection:** `oura` — OAuth (existing connection; not shared with other
  defs). PATs were deprecated by Oura December 2025 — OAuth only for new
  connects; token refresh handled.
- **evidence:** official-docs — cloud.ouraring.com/oauth/applications
  (self-service app registration), api.ouraring.com/v2 (documented
  collections)
- **effort / priority:** S / P0
- **needs:** none

## What it is

Smart-ring health tracker: sleep, readiness, activity, continuous heart
rate, SpO2, stress, and a stack of Oura-computed composite scores. Matters
because Oura does **not** push everything to Apple Health — the 5-minute
HRV time-series, exact-timing sleep stages, and readiness/resilience/
cardiovascular-age scores are Oura-API-only fields.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Daily summaries | membership | daily_sleep, daily_activity, daily_readiness, daily_spo2, daily_stress, daily_cardiovascular_age, daily_resilience | official docs |
| Continuous heart rate | membership | 5-min interval HR/HRV time-series | official docs |
| Sessions & events | membership | sleep (per-session detail), workout, tag, enhanced_tag, rest_mode_period, sleep_time, vo2_max | official docs |
| Account singletons | membership | personal_info; ring_configuration (PAT-only endpoint — OAuth path skips it, no scope covers it) | official docs + shipped code |

All optional; collections that 401 or return empty simply produce no files.

## Access & auth

- OAuth 2.0: authorize + token at cloud.ouraring.com/oauth/*, data at
  `api.ouraring.com/v2/usercollection/{collection}`.
- Rate limit unpublished but generous for personal pulls; the collector
  budgets requests per watcher pass anyway.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `health/oura/<collection>.jsonl` (whole-file rewrite,
  sorted by key); `health/oura/heartrate/YYYY-MM.jsonl` (month-partitioned
  — ~300 records/day); `personal_info.json` / `ring_configuration.json`
  singletons; `index.md` summary regenerated each sync.
- **Contract layer:** none yet — `health/` is a document-domain. Records
  are **upserted by key** (daily collections by `day`, events by `id`,
  heartrate by `timestamp`) because Oura recalculates recent days after
  late ring syncs.
- **Cursor:** per-collection in `.trove/oura-sync.json`; full-history
  backfill walks windows backward to account start, resumable.

## Build plan

Already shipped: `crates/trove-core/src/oura.rs` (collector) +
`crates/trove-core/src/sync/oura.rs` (OAuth/token store), registered in
`INTEGRATIONS` + `CONNECTIONS`. No pipeline work queued; promote-to-✅ is
David's confirmation pass against his live account.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| All v2 collections | 🧪 built pre-pipeline (fixture-tested, running live) | David: confirm `health/oura/` files current + hub last-data fresh after a ring sync, then promote to ✅ |
| OAuth refresh | 🧪 built pre-pipeline | leave connected >24h; confirm sync continues without re-auth |
| Backfill-to-account-start | 🧪 built pre-pipeline | fresh vault + connect; confirm history walks back past 30 days and cursor resumes after interrupt |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Oura Ring
(L830–L836). Feasibility 🟢 high. Backfill from account creation is
supported (API serves arbitrary date ranges) and built. Oura syncs basic
sleep/activity summaries to Apple Health but strips vendor scores — the
direct API is strictly richer than the Apple Health path.
