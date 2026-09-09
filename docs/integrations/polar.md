# Polar

- **id:** `polar`
- **domains:** `health/` (contract: **document** — the per-metric CSV +
  per-source raw shape already built; Phase 3 writes the spec page without
  redesigning it). Workout records route here **whole**, embedded GPS
  routes included (the location view joins at read time).
- **status:** 🧪 built (fixture-tested, not validated — needs a real Polar Flow OAuth login to confirm AccessLink v4 shapes)
- **unavailable_reason:** none
- **behavior:** Periodic (AccessLink v4 poll) + Import (bulk-export ZIP /
  per-session FIT/TCX/GPX/CSV as backfill)
- **connection:** `polar` — OAuth (self-service registration at
  polar.com/developers/, just a Polar Flow account needed; not shared with
  other defs). The file imports need no connection.
- **evidence:** official-docs — Polar AccessLink API v4
  (polar.com/polar-api-v4/), self-service, no commercial approval; official
  bulk export via support.polar.com
- **effort / priority:** M / P2
- **needs:** privacy (workout GPS = location trails — opt-in with explicit
  acknowledgement)

## What it is

Polar makes sports watches and heart-rate monitors with a large base among
serious endurance athletes. Basic activity/HR/sleep/weight reach Apple
Health, but Polar's derived training science — Nightly Recharge (ANS
recovery), Training Load Pro, Cardio Load, Running Performance, muscle
load, orthostatic/fitness tests — is API-only and exists nowhere else in
the vault.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Training sessions | any Flow account | session summaries + FIT/TCX/CSV files (HR, GPS, pace) | official docs |
| Sleep + Nightly Recharge | any | sleep stages, ANS recovery score | official docs |
| 24/7 HR + HRV | any | continuous HR, HRV | official docs |
| Daily activity | any | steps, calories, activity goal | official docs |
| VO2Max / fitness tests | any | test results | official docs |
| Bulk export ZIP | any (manual) | JSON per session + bulk JSON — **no derived data** (see notes) | official docs |

All optional in the contract; no tier-specific code paths.

## Access & auth

- OAuth 2.0 via AccessLink v4 at www.polar.com/polar-api-v4/; free
  developer registration at polar.com/developers/.
- Bulk export: support.polar.com → "download all your data" → ZIP.
  Individual sessions: Polar Flow web → session → Export (FIT/TCX/GPX/CSV).
- **Key caveat:** the bulk JSON export does NOT include derived algorithm
  data (Nightly Recharge, activity/sleep summaries) — those are API-only,
  so the import path alone is incomplete; the OAuth pull is the real
  integration.
- The Polar BLE SDK (live device data) requires the app running against
  hardware — violates the standalone rule; skip entirely.
- No TCC, no local files. Standalone-clean (plain HTTPS + file drops).

## Vault mapping

- **Raw layer:** `health/polar/` — per-collection JSONL (sessions, sleep,
  recharge, daily-activity), Oura-style; imported FIT/TCX originals kept
  under `health/polar/imports/`.
- **Contract layer:** metrics join the per-metric layout as built
  (`health/heart-rate/`, `health/sleep/`, … `YYYY-MM.csv` + `daily.csv`);
  workouts land whole in `health/workouts/YYYY-MM.csv` with the GPS route
  embedded in the raw session record.
- **Dedupe:** sessions by session id; daily collections by day; cursor in
  `.trove/polar-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/polar.rs`: `DEF` (Periodic + import hook),
   `CONNECTION` (OAuth, baked + BYO creds), pull hook with backfill
   windows.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. FIT parsing via the `fitparser` crate (shared with Garmin/COROS/Strava
   bulk imports — build it as a common helper, not per-module).
4. Fixtures from AccessLink v4 documented examples + a sample bulk-export
   ZIP; tests with unique temp dirs.
5. Privacy gate: ships opt-in (GPS trails in workouts).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Bulk-export import | — | request a real Polar data ZIP, drop on import box, confirm `health/polar/` + workout rows + hub last-data |
| API sync incl. Nightly Recharge | — | register at polar.com/developers/, OAuth in connect card, Sync now; needs a real Polar wearer (David is not one — any real user's run validates) |

## Build notes (2026-06-17)

Implemented as a raw-only Periodic poller (`crates/trove-core/src/polar.rs`),
dedicated OAuth connection (`polar`, port 38835 = 38580+255, Basic auth).
Four JSONL collections: sessions / sleep / recharge / activity, each deduped
by its stable id key. Cursor in `.trove/polar-sync.json`, per-collection
watermarks. Import fn (`run_import`) available for FIT/TCX/GPX/CSV/ZIP file
drops; primary DEF is Periodic (WHOOP pattern).

Contract decision: raw-only for now. The brief calls for a "per-metric contract
layer" as a future Phase-4 pioneer step — no bound contract exists for Polar's
mix of workout/sleep/recovery/activity (health-medical.Observation would fit
individual readings like HR/HRV only; the sessions shape is richer). Shipping
raw-only with full fidelity; contract binding deferred.

Validation matrix — updated:

| Capability | Status | Notes |
|---|---|---|
| Bulk-export import | built (raw copy only; FIT parsing not yet implemented) | Run file drop; confirm `health/polar/imports/` |
| API sync incl. Nightly Recharge | built | Connect via polar.com/developers; needs real Polar wearer to validate |

## Research notes

`integrations-research.md` → Health: Wearables & Biometrics §Polar
(L878–L884). Feasibility 🟢 high — one of the few wearable vendors with
genuinely self-service API access. Sequence after WHOOP/Withings (P1
wearables) per catalog priority; Strava can serve as a fallback aggregator
for Polar users in the meantime (sessions only, no recovery data).
