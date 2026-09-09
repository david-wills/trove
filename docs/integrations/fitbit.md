# Fitbit

- **id:** `fitbit`
- **domains:** `health/` (contract: **document** — the as-built per-source
  raw shape; Phase 3 writes the spec page, no redesign)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (OAuth poll)
- **connection:** `fitbit` — OAuth via a Google Cloud project with the
  Google Health API enabled. **Distinct from the existing `google`
  connection** (different API surface, scopes, and review regime — do not
  fold into the Google bundle). Not shared with other defs.
- **evidence:** official-docs — Google Health API (the successor; the
  legacy Fitbit Web API shuts down September 2026, the separate Google Fit
  REST API also dies late 2026 — build against neither)
- **effort / priority:** M / P1
- **needs:** none

## What it is

Fitness trackers with tens of millions of users; many early wearable
adopters have years of Fitbit history that never reached Apple Health.
Fitbit syncs only basic steps/sleep/HR over — intraday heart rate, HRV,
Active Zone Minutes, and VO2Max training metrics are API-only.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Activity bundles | all accounts | steps, calories, floors, distance, active-zone-minutes | official docs (Google Health API) |
| Sleep | all accounts | stages, durations | official docs |
| Heart | all accounts | intraday HR, HRV, resting HR | official docs |
| Vitals | device-dependent | SpO2, respiratory rate, body temp, blood glucose | official docs |

All optional; users without the relevant sensor simply yield no rows for
that stream. No tier code paths.

## Access & auth

- Google Health API: Google Cloud project → enable the API → OAuth 2.0.
  Legacy Fitbit OAuth (FOT) no longer accepts new integrations.
- CASA security review is required above 100 users — fine for early
  builds, but a real gate for a distributed app shipping baked
  credentials (see Research notes).
- No fees under 100 users. Rate limits per Google Health API docs —
  verify in the Phase 4 loop.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `health/fitbit/<stream>.jsonl` for daily summaries;
  `health/fitbit/heartrate/YYYY-MM.jsonl` for intraday series
  (month-partitioned, Oura-style).
- **Contract layer:** none — `health/` is a document-domain; per-source
  raw is the shape.
- **Dedupe:** keyed upserts — dailies by date, intraday by timestamp;
  cursor in `.trove/fitbit-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/fitbit.rs`: `DEF` (Periodic) +
   `CONNECTION` (OAuth) in `sync/fitbit.rs`. **v-next only:** target the
   Google Health API exclusively; never the legacy Fitbit Web API or the
   Google Fit REST API (both shut down in 2026).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the Google Health API documented response shapes;
   parser/store/cursor tests, unique temp dirs.
4. Credentials: BYO Google Cloud project first (per the ConnectSpec
   baked+BYO model); a baked credential needs the CASA-review question
   answered before it can scale past 100 users — Needs-David decision at
   ship time, not a build blocker.
5. Backfill: pull history by date range back to account start, resumable
   cursor.

## Build notes (Phase 4)

- Module: `crates/trove-core/src/fitbit.rs` (Periodic poller) + `src/sync/fitbit.rs` (OAuth).
- Connection id `"fitbit"` added to CONNECTIONS in `integrations.rs`.
- Raw-only domain (`health/`): no contract binding — `contract_mode = raw-only`.
- Streams: steps, distance, floors, active-zone-minutes, active-energy-burned, resting-heart-rate, heart-rate-variability, sleep (daily); heartrate (intraday, month-partitioned).
- Cursor: per-stream watermark + backfill cursor in `.trove/fitbit-sync.json`. Watermark advanced only after full window drain; backfill exits after 3 consecutive empty windows.
- OAuth port: 38653 (unique to this integration); PKCE + `access_type=offline` + `prompt=consent` for refresh tokens.
- CASA >100-user review flagged as `Needs-David` (a scale gate, not a build blocker).
- 28 unit tests green (cargo test -p trove-core fitbit::).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| OAuth connect + daily pulls | — | connect a real Fitbit/Google account; Sync now; confirm `health/fitbit/` rows + hub last-data |
| Intraday HR partitioning | — | confirm `heartrate/YYYY-MM.jsonl` only rewrites touched months on incremental sync |
| Historical backfill | — | fresh connect on an account with multi-year history; confirm walk-back completes and resumes after interrupt |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Fitbit /
Google Health API (L862–L868). Feasibility 🟡 medium — the API is
legitimate and self-service at personal scale, but the Google Cloud
project requirement adds friction versus simpler OAuth apps, and the
September 2026 Fitbit-API shutdown means timing matters: anything built
against legacy endpoints is dead on arrival, so this brief is pinned to
the Google Health API from the first line of code. The CASA >100-user
review is the one open scale question for Trove's
built-for-anyone distribution model.
