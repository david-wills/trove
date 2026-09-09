# Withings

- **id:** `withings`
- **domains:** `health/` (contract: **document** — the as-built per-source
  raw shape; Phase 3 writes the spec page, no redesign)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (OAuth poll; readings land at weigh-in/wake
  cadence, hourly poll is plenty)
- **connection:** `withings` — OAuth (new connection; free self-service
  Public API tier at developer.withings.com). Not shared with other defs.
- **evidence:** official-docs — developer.withings.com, wbsapi.withings.net
  endpoints (measure/sleep/activity/heart/user) documented; dashboard CSV
  export as fallback
- **effort / priority:** M / P1
- **needs:** privacy (medical-grade readings — ECG signals, blood
  pressure, body composition — ship opt-in with explicit acknowledgement)

## What it is

Connected-health device maker: smart scales (Body/Body+/Body Cardio/Body
Scan), ScanWatch, blood-pressure monitors, sleep mat, thermometer — all
under **one API**. Uniquely valuable because body-composition trends, BP
history, ECG signals, pulse-wave velocity/vascular age, and sleep-mat
respiratory data are not fully represented in Apple Health exports (only
weight/BMI/BP/sleep/steps sync over). One brief covers scale-only users
and full-stack users alike — same API, same collector.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Measures | any device | weight, fat%, muscle/bone mass, hydration, visceral fat, BMR, BP, temp, pulse-wave velocity, vascular age | official docs (/measure getmeas) |
| Sleep | sleep mat / ScanWatch | sleep summary, stages, HRV, respiratory rate, snoring | official docs (/sleep) |
| Activity | trackers / ScanWatch | steps, distance, calories, HR | official docs (/activity) |
| Heart | ScanWatch / Body Cardio+ | ECG signal + AFib classification | official docs (/heart) |
| Body Scan extras | Body Scan device; some newer metrics may need the premium API tier | segmental body comp, nerve-health EDA | official docs |

All optional — a scale-only user simply yields measures rows. The
"Intelligence/Scores" API (vitality score, disease risk) is listed
"coming soon"; ignore until real.

## Access & auth

- OAuth 2.0 via developer.withings.com (free Public API tier, no
  commercial approval). Data at `wbsapi.withings.net/{measure,sleep,
  activity,heart,user}`.
- Refresh token valid 1 year — handle refresh; surface a reconnect hint
  rather than failing silently if it lapses.
- Fallback: Withings Health dashboard → Settings → Download my data → CSV
  (weight) — acceptable manual backfill, not the primary path.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `health/withings/<collection>.jsonl` (measures, sleep,
  activity, heart), keyed upserts by measurement id/timestamp;
  month-partition if volume warrants (activity dailies won't; ECG signal
  payloads might — decide at fixture time).
- **Contract layer:** none — `health/` is a document-domain; per-source
  raw is the shape.
- **Dedupe:** Withings measure group ids (`grpid`) / record timestamps as
  `guid`; cursor in `.trove/withings-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/withings.rs`: `DEF` (Periodic) +
   `CONNECTION` (OAuth) in `sync/withings.rs` modeled on `sync/oura.rs`.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented endpoint response shapes (measures with
   multiple measure types per group; sleep-stage series; ECG record);
   parser/store/cursor tests, unique temp dirs.
4. Privacy gate: ships opt-in with explicit acknowledgement (ECG/BP/body
   composition are medical-grade detail).
5. Backfill: API serves history by date range — walk forward from
   2010-01-01 (WITHINGS_EPOCH) in successive 90-day windows, Oura-style
   resumable cursor; empty windows still advance the watermark so cold-start
   accounts reach their actual data.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Measures (weight/body comp) | ✅ unit tests | `cargo test -p trove-core withings::` — fixture-verified mapping for weight/fat/BP/HR |
| Sleep / heart / activity | ✅ unit tests | stub API tests; real device needed for live validation |
| Token refresh at 1yr | ✅ unit tests | `fresh_token` path tested (unexpired pass-through + expired-no-refresh reconnect) |
| Contract observations | ✅ unit tests | 5-row fixture across 2 groups; dedup verified; LOINC codes confirmed |

## As-built notes

- **contract_mode:** `reuse-bound` — Withings measures (weight, BP, HR, fat%, etc.)
  map to `health-medical.Observation` rows in `health/medical/withings/observations/YYYY-MM.jsonl`.
  The `guid` is `"<grpid>-<type>"` — stable across re-pulls (grpid is the measure-group id,
  type is the measure type code).
- **Sleep / activity / heart:** raw-only (`health/withings/{sleep,sleep_summaries,activity,heart}/raw/`).
  Sleep summaries and stages fit an unbound sibling draft (no bound contract exists yet).
- **OAuth port:** 38670 (assigned; not yet registered in any Withings app).
- **Redirect URI:** `http://localhost:38670/callback`.
- **Measure type codes confirmed** from `python_withings_api` + LOINC browser:
  weight=1 (29463-7), height=4 (8302-2), fat_ratio=6 (41982-0), diastolic_bp=9 (8462-4),
  systolic_bp=10 (8480-6), heart_rate=11 (8867-4), temperature=12 (8310-5), SpO2=54 (59408-5),
  muscle_mass=76, hydration=77, bone_mass=88, pulse_wave_velocity=91, VO2max=123 (60842-2).
- **Needs-login:** a Withings developer app must be registered at developer.withings.com
  (free Public API tier). No baked credentials until David registers an app and sets
  `TROVE_WITHINGS_CLIENT_ID` / `TROVE_WITHINGS_CLIENT_SECRET` at build time.

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Withings
(L846–L852) + standalone-scale entry (L902–L908, same API — folded into
this one brief per the combine-by-provider rule) + "Nutrition, Medical,
Labs" §Withings (L1201–L1207). Feasibility 🟢 high across all three
entries. Pairs naturally with Oura (already built) for whole-body
coverage. BeamO stethoscope data is also API-only — same collector picks
it up if/when fields appear.
