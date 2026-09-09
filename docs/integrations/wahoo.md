# Wahoo Fitness

- **id:** `wahoo`
- **domains:** `health/` (workouts route here **whole**, embedded GPS routes
  included — contract: **document**, the per-source raw + per-metric shape as
  built; the location view joins routes at read time per the taxonomy rule)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (API v1 poll for new workouts, watermark cursor) +
  FIT download per workout
- **connection:** `wahoo` — OAuth 2.0 (standard, **not** OAuth 1.0a unlike
  Garmin; self-service app registration at `cloud-api.wahooligan.com`; compiled-in
  app credentials + BYO per ConnectSpec). New connection, not shared with other
  defs.
- **evidence:** official-docs — `cloud-api.wahooligan.com` / `api.wahooligan.com`
  documented OAuth 2.0 API: `GET /v1/workouts` list, `GET /v1/workouts/{id}`
  with a FIT file URL under the `file` key; `workout_summary` webhook events;
  10-token-per-user cap from 2026-01-01.
- **effort / priority:** M / P2
- **needs:** privacy (GPS workout routes = location trails — opt-in with
  explicit acknowledgement) · Needs-login (validation only — build proceeds from
  documented shapes)

## What it is

Wahoo makes ELEMNT GPS bike computers and KICKR smart trainers. Its self-serve
OAuth 2.0 cloud API returns the workout list plus a downloadable FIT file per
activity — full GPS, power, and heart-rate streams that don't reach Apple
Health. Most Wahoo users **auto-sync to Strava**, so the Strava aggregator pull
already captures them; this direct integration is for users who don't use Strava
or who want the raw FIT files. Build after Strava for that reason.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Workout list | any account | workout summaries (type, time, duration, distance) | official docs |
| Workout detail | any | per-workout detail | official docs |
| FIT file per workout | any | full GPS + power + HR + cadence time series (FIT under the `file` key) | official docs |
| Workout webhook | any (needs public callback) | `workout_summary` push on completion | official docs |

All optional; no tier-specific code paths.

## Access & auth

- OAuth 2.0 at `api.wahooligan.com` — scopes `workouts_read`, `offline_data`.
  `GET /v1/workouts` lists, `GET /v1/workouts/{id}` returns the FIT URL; download
  and decode with the shared `fitparser` helper (same path Garmin/Strava/COROS
  imports use).
- Token cap: 10 unrevoked tokens per user from 2026-01-01 — generous for
  personal use.
- Webhooks (`workout_summary`) need a publicly reachable callback URL — not
  viable for a local app; polling is the path.
- No TCC, no local files. Standalone-clean (plain HTTPS + a compiled-in FIT
  decoder).

## Vault mapping

- **Raw layer:** `health/wahoo/` — `activities/YYYY-MM.jsonl` (workout summary +
  detail per activity) and the downloaded FIT originals under
  `health/wahoo/imports/`. GPS routes stay embedded in the workout record
  (routed whole per the taxonomy; the location view joins them at read time —
  the research doc's geo path predates the taxonomy and does not apply).
- **Contract layer:** workouts land in `health/workouts/…` as built (the
  document shape, merging at read time with Apple Health / Strava workouts).
  Double-count hazard: a Wahoo ride that also auto-synced to Strava arrives via
  both — dedupe at read time by start-time proximity; raw stays complete on
  both sides.
- **Dedupe:** workout id as `guid`; cursor in `.trove/wahoo-sync.json`,
  rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/wahoo.rs`: `DEF` (Periodic), `CONNECTION`
   (OAuth 2.0, baked + BYO creds per ConnectSpec), `pull` hook (list → detail →
   FIT download).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. FIT decode via the shared `fitparser` helper — no new parser, reuse the
   Garmin/Strava code path.
4. Fixtures from `cloud-api.wahooligan.com` documented responses + a sample FIT
   file; parser + store + cursor tests, unique temp dirs.
5. Privacy gate: ships opt-in (GPS trails) with explicit acknowledgement.
6. Sequence **after Strava** — most Wahoo users auto-sync there, so Strava
   captures them first; Wahoo direct is the fill-in for non-Strava users.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Workout list + detail | ✅ unit-tested | OAuth in the connect card; Sync now; confirm `health/wahoo/activities/` rows + hub last-data |
| FIT download + decode | ✅ unit-tested (download path) | confirm FIT originals under `health/wahoo/imports/` decode to GPS/power/HR streams |
| Strava de-dupe | — (read-time) | for a user with Wahoo→Strava auto-sync, confirm read-time dedupe avoids double-counting |

## Build notes (2026-06-21)

- Implemented as a Periodic OAuth 2.0 poller (30-minute cadence).
- New `pub static CONNECTION` (id=`wahoo`) with `redirect_port: 38852`.
- Watermark cursor stored in `.trove/wahoo-sync.json` (`updated_after` field, RFC3339).
- Raw workouts land in `health/wahoo/activities/YYYY-MM.jsonl`; FIT files in `health/wahoo/imports/<id>.fit`.
- FIT downloads budgeted at 50/pass to avoid stalling a large backfill.
- 7 unit tests (port assertion, empty-list, new-workout-stored+FIT-downloaded, rerun-dedup, watermark filtering, FIT-URL-absent, watermark-helper). All green.
- Brief's note about FIT parsing via the shared `fitparser` helper: FIT files are stored raw; fitparser decode for vault indexing is a future read-time pass (same pattern as Garmin).

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Wahoo Fitness
(L2332–L2338); related Strava aggregator notes (L50, L123, L896). Feasibility
🟢 high — official, well-documented, self-serve OAuth 2.0 (notably *not* OAuth
1.0a, unlike Garmin). FIT files give full GPS + power + HR. Lower priority than
Strava, which aggregates Wahoo (plus Garmin/Apple Watch/Polar/Suunto) into one
pull — build Strava first, then Wahoo direct for users who want raw FIT or skip
Strava. 10-token/user cap (2026) is generous for personal use.
