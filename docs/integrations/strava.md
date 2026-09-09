# Strava

- **id:** `strava`
- **domains:** `health/` (workouts route here **whole**, embedded GPS
  streams included — contract: **document**, the per-metric CSV +
  per-source raw shape as built; Phase 3 writes the spec page) +
  `social/strava/` (kudos/followers/segment standing — per-source **raw**;
  these are not posts, so the pending Phase 3 social-posts contract does
  not apply to them)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (API v3 poll for new activities) + Import
  (bulk-export ZIP for history backfill)
- **connection:** `strava` — OAuth (`athlete:read_all` scope; compiled-in
  app credentials + BYO per ConnectSpec; self-service registration at
  developers.strava.com). Not shared with other defs.
- **evidence:** official-docs — developers.strava.com (API v3, documented
  endpoints + rate limits; API Agreement updated June 2026, still live);
  official bulk export (Settings → My Account → Download)
- **effort / priority:** M / P1
- **needs:** privacy (GPS location trails — opt-in with explicit
  acknowledgement) · Needs-login (validation only — build proceeds from
  documented shapes)

## What it is

The de facto social home for GPS workouts, with an enormous user base.
Strava is an **aggregator hub**: it collects activities from Garmin, Apple
Watch, Wahoo, Suunto, Polar — so one pull can cover a multi-device
athlete's entire workout history, including devices they no longer own and
vendors whose own APIs are approval-gated. GPS streams, power data, and
segment efforts never reach Apple Health.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Activity list | any account | name, date, type, distance, moving/elapsed time, elevation, pace/HR/power summaries (`/athlete/activities`) | official docs |
| Activity detail + laps | any | full per-activity detail (`/activities/{id}`, `/laps`) | official docs |
| Raw streams | any | time-series latlng/time/altitude/HR/cadence/power/velocity (`/activities/{id}/streams`) — one call per activity | official docs |
| Bulk export ZIP | any (manual) | activities.csv + original FIT/GPX per activity, routes.csv, profile.json, photos | official docs |
| Social layer | any, API-only (not in bulk export) | kudos, followers/following, clubs, athlete stats | official docs |

All optional in the contract; no tier-specific code paths.

## Access & auth

- OAuth 2.0 (`POST strava.com/oauth/token`); API v3 endpoints above.
- Rate limits: 200 req/15 min + 2,000/day per athlete as of June 2026 (the
  earlier wearables entry recorded 100/1,000 — Phase 4 confirms; either
  way, comfortable for a personal pull as long as per-activity stream
  fetches are budgeted, Oura-style).
- Bulk export: Settings → My Account → "Download or Delete Your Data" →
  ZIP emailed within hours. Prefer it for the initial backfill — saves the
  API quota for ongoing sync.
- Webhooks need a publicly reachable callback URL — not viable for a local
  app; polling is fine.
- API Agreement: prohibits bulk resale / public display without
  attribution — irrelevant to a local vault, but record compliance in the
  def's notes.
- No TCC, no local files. Standalone-clean.

## Vault mapping

- **Raw layer:** `health/strava/` — `activities/YYYY-MM.jsonl` (summary +
  detail + streams per activity, GPS embedded, routed whole per the
  taxonomy); bulk-export originals (FIT/GPX) under
  `health/strava/imports/`. Social layer: `social/strava/` —
  `kudos.jsonl`, `followers.jsonl`, `stats.json`, raw-only.
- **Contract layer:** workouts land in `health/workouts/YYYY-MM.csv` as
  built (merging at read time with Apple Health workouts — note the
  double-count hazard: an Apple Watch workout can arrive via both sources;
  dedupe at read time by start-time proximity is a read-feature note, raw
  stays complete on both sides).
- **Dedupe:** activity id as `guid`; cursor in `.trove/strava-sync.json`,
  rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/strava.rs`: `DEF` (Periodic + import
   hook), `CONNECTION` (OAuth, baked + BYO creds), pull hook (list →
   detail → streams, request-budgeted).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Bulk-export ZIP import: activities.csv parser + FIT/GPX via the shared
   `fitparser` helper (same code path Garmin/Polar/COROS imports use).
4. Fixtures from developers.strava.com documented responses + a sample
   export ZIP; tests with unique temp dirs.
5. Privacy gate: ships opt-in (GPS trails).
6. Social pull is a second def sharing the `strava` connection
   (`strava-social`), default-off — most users want workouts only.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Bulk-export import | 🔲 deferred | Brief called for a separate Import DEF (`strava-import`). No stub exists in INTEGRATIONS; bulk-export parser not built. Needs-sample (activities.csv column order is folklore — no sample on disk). |
| API sync + streams | 🧪 built | OAuth in the connect card, Sync now, confirm new activities with GPS streams under `health/strava/activities/YYYY-MM.jsonl` + stream JSON under `health/strava/streams/{id}.json`; David has a Strava account — direct validation possible |
| Social layer | 🔲 deferred | Brief called for a second DEF (`strava-social`). No stub in INTEGRATIONS; not built here. |

## Build notes (Phase-4 follower, 2026-06-16)

- **Behavior:** `Periodic` (hourly API v3 poll). The Import + strava-social variants require separate DEF registrations not present in integration-staging; deferred.
- **Connection:** new `strava` OAuth ConnectionDef, port 38666. Integrator must add `&crate::strava::CONNECTION,` to CONNECTIONS.
- **Raw layer:** `health/strava/activities/YYYY-MM.jsonl` (detail merged into summary, month-partitioned, append-only guid-deduped by id); `health/strava/streams/{id}.json` (per-activity stream, written once, stream fetch budgeted at 40/pass).
- **Contract:** raw-only (no health-workouts contract exists; brief says "document as built"). The brief's health/workouts contract is an unbound concept — no reusable bound contract to sub-bind.
- **Tests:** 16 passing, all offline, unique temp dirs. Token never in the cursor file (asserted).
- **Narrowing vs brief:** Import box and strava-social DEF deferred (no stubs in INTEGRATIONS); build is the Periodic API sync only.

## Research notes

`integrations-research.md` → Health: Wearables & Biometrics §Strava
(L894–L900), Geolocation & Travel §Strava (L2268–L2274), Social §Strava
social layer (L4128–L4134). Feasibility 🟢 high across all three. Strava
provides **no** sleep/recovery/passive metrics — workouts only; it
supplements rather than replaces Garmin/Polar briefs. June 2026 API churn:
club endpoints removed Sep 2026, segment exploration moving to an
Extended Access Tier — both irrelevant to personal data. Build before
Wahoo (most Wahoo users auto-sync here).
