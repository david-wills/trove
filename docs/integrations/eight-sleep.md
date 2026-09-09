# Eight Sleep

- **id:** `eight-sleep`
- **domains:** `health/` (contract: **document** — per-metric CSV +
  per-source raw, as built; Phase 3 writes the spec page without
  redesigning it)
- **status:** 🧪 built (fixture-tested; Needs-login to validate — see Build status + validation matrix below)
- **unavailable_reason:** none
- **behavior:** Periodic (poll the unofficial cloud API for new sleep
  sessions; watermark cursor)
- **connection:** `eight-sleep` — TokenPaste-style account credentials
  (email/password → OAuth2 token against app.eightsleep.com, per the
  community client). **Unofficial** — the connect card must say so plainly.
  Not shared with other defs.
- **evidence:** community reverse-engineered, well-maintained —
  github.com/lukas-clarke/eight_sleep (Home Assistant integration, active
  as of 2026); Free Sleep local path github.com/throwaway31265/free-sleep
  (docs-only here)
- **effort / priority:** L / P2
- **needs:** Needs-login (an Eight Sleep account + Pod to validate;
  build proceeds from the community client's documented shapes)

## What it is

Smart-mattress cover ("Pod") that records sleep biometrics every ~2
seconds: beat-by-beat HR, HRV, breath rate, toss/turn events, bed/room
temperature. Eight Sleep pushes only summary sleep (total, score, stages)
to Apple Health — the full biometric stream is API-only, which is exactly
the slice worth collecting.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Sleep sessions | subscription bundled w/ Pod | stages, score, duration | community client |
| Biometric time series | same | HR, HRV, breath rate intervals | community client |
| Bed environment | same | bed temp, room temp, heating/cooling events | community client |

All optional in the contract; if an API change drops a field, rows simply
omit it — graceful degradation, never silent failure.

## Access & auth

- **No official public API.** Path A (build): reverse-engineered cloud API
  used by the lukas-clarke Home Assistant integration — OAuth2 against
  app.eightsleep.com, then REST pulls. Plain HTTPS: passes the standalone
  test (it's HTTP calls, not a running-app dependency) but is ToS-gray and
  fragile at API changes — ship with explicit "unofficial" disclosure and
  graceful failure (clear hub error state, never crash the runner).
- Path B (docs-only): Free Sleep — rooting the Pod 3 exposes a local
  REST/SQLite server (`/persistent/free-sleep-data/free-sleep.db`,
  `/api/metrics/vitals`). Full local fidelity, no cloud, but requires the
  user to root their device and keep its server running — document it for
  power users as a possible labeled opt-in M6; not the default collector.
- No TCC, no local files for path A.

## Vault mapping

- **Raw layer:** `health/eight-sleep/raw/YYYY-MM.jsonl` — API session and
  interval objects, full fidelity.
- **Contract layer:** per-metric CSVs per the as-built health shape (sleep
  sessions; HR/HRV/breath-rate series as interval rows), per the pending
  Phase 3 health spec page. Overflow in `extra`.
- **Dedupe:** session id as `guid`; cursor in
  `.trove/eight-sleep-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/eight_sleep.rs`: `DEF` (Periodic, a few
   times daily — sleep data lands once a night), `CONNECTION` (credentials
   form with the unofficial-API disclosure in the setup copy, per the
   SimpleFIN affordance rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the community client's documented response shapes
   (session + interval variants); parser + store + cursor tests, unique
   temp dirs.
4. Failure design is first-class: on auth/shape breakage, surface a clear
   "unofficial API changed" hub state with a link to re-check — never
   retry-loop silently.
5. Free Sleep local path: research-notes only; revisit on demand.

## Build notes (2026-06-21)

- **Status:** 🧪 built (follower fan-out)
- **Auth:** TokenPaste (`email:password`), exchanged immediately for a password-grant access token via `POST https://auth-api.8slp.net/v1/tokens`. Community client_id/secret baked in (public knowledge from lukas-clarke/eight_sleep). No refresh token in the documented flow — token expiry prompts reconnect.
- **Contract:** `reuse-bound` — dual sink:
  - `health-medical.Observation` for HR timeseries (LOINC 8867-4), HRV (LOINC 80404-7), respiratory rate (LOINC 9279-1) under `health/medical/eight-sleep/observations/YYYY-MM.jsonl`
  - `home.HomeReading` for bed temperature (`temperature_bed`/C) and room temperature (`temperature`/C) under `home/eight-sleep/YYYY-MM.jsonl`
  - Raw full-fidelity day objects under `health/eight-sleep/raw/YYYY-MM.jsonl`
- **Cursor:** `.trove/eight-sleep-sync.json` — watermark is the last synced `YYYY-MM-DD`. Cold start backfills 90 days; WINDOW_DAYS=7 chunks.
- **CONNECTION:** new `"eight-sleep"` TokenPaste — integrator must add `&crate::eight_sleep::CONNECTION,` to CONNECTIONS in integrations.rs.
- **Tests:** 18 tests, all green. Pure mapping + mock API + dedup + watermark + error-path coverage.
- **Narrow vs brief:** HR timeseries collected from `sessions[].timeseries.heartRate` (interval rows). HRV and resp rate are per-session summary scalars (not interval timeseries — the brief didn't distinguish). Sleep score and stages captured in raw only (no contract column maps them without forcing; raw preserves full fidelity).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Sessions + biometrics | 🧪 built | enter real Eight Sleep credentials in the connect card; Sync now; confirm rows in `health/eight-sleep/` + `health/medical/eight-sleep/` + hub last-data after a slept night |
| Bed/room temperature | 🧪 built | confirm rows in `home/eight-sleep/` with `temperature_bed` and `temperature` metrics |
| Breakage handling | 🧪 built | point fixture tests at a mutated response; confirm graceful hub error, no partial writes |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Eight Sleep
Pod (L934–L940). Feasibility 🟡 medium. Pod 3 is the confirmed-supported
device. Cross-cutting note #5: unofficial APIs (Eight Sleep, Renpho,
Amazfit) are ToS-gray and fragile — Eight Sleep's is the only practical
path to its detailed biometrics, hence worth it where the others aren't.
Standalone-line note (research L157): unofficial cloud APIs pass; the
rooted-Pod local server does not as a default.
