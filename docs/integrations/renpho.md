# Renpho

- **id:** `renpho`
- **domains:** `health/` (contract: **document** — per-metric CSV + per-source
  raw, as built; Phase 3 writes the spec page without redesigning it)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (manual CSV export from the Renpho iOS app)
- **connection:** none (the import path needs no login; the unofficial cloud
  API would need account credentials, and is explicitly *not* the plan)
- **evidence:** low — community/unofficial only. Reverse-engineered cloud API
  at renpho.qnclouds.com (renpho-api PyPI / neilzilla/hass-renpho lineage); no
  Rust library; the app's manual CSV export is undocumented. sample-required.
- **effort / priority:** L / P2
- **needs:** Needs-sample (CSV format undocumented — parser built last, from a
  real export) · Needs-David (**icebox** — build only on real demand; weight
  already reaches Apple Health)

## What it is

Renpho makes inexpensive Bluetooth smart scales with a companion app that
records weight plus bioimpedance body composition (fat %, muscle mass, bone
mass, water %). Weight and BMI sync to Apple Health — so the shipped Apple
Health import already captures that for iPhone users. The *only* data the
existing pipeline misses is the body-composition detail, which Renpho does
not push to Apple Health. That gap is real but narrow, and every automated
path to it is fragile — hence iceboxed.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Weight / BMI | none | weight, BMI w/ timestamps | already covered via Apple Health sync (built `health` import) |
| Body composition | none | fat %, muscle mass, bone mass, water % | community: unofficial API / manual CSV; sample-required |

All optional in the contract; a weight-only export simply carries no
body-comp columns. No paid tiers in play.

## Access & auth

- **Planned path (if built):** manual CSV export from the Renpho iOS app —
  user-initiated, no credentials, no TCC. Format undocumented; parser is
  written against a real sample (Needs-sample).
- **Rejected path:** unofficial cloud API — `POST
  https://renpho.qnclouds.com/api/v3/users/sign_in.json` (app_id + account
  credentials) then measurement GETs. Reverse-engineered, ToS-gray, endpoints
  churn, no Rust lib (would need a re-implementation). Not worth it for a P2
  icebox source; revisit only on demand.
- Third-party aggregators (Terra) have a Renpho integration — a cloud
  middleman, out of scope.
- Standalone-clean either way; the import path is fully offline.

## Vault mapping

- **Raw layer:** `health/renpho/raw/` — the imported CSV(s) as received,
  plus parsed rows in `health/renpho/YYYY-MM.jsonl` if normalization happens.
- **Contract layer:** the documented health shape (per-metric CSV, as built):
  weight rows join the existing body-mass metric stream (dedupe against
  Apple-Health-sourced rows by timestamp+value); body-comp metrics (fat %,
  muscle, bone, water) get their own per-metric CSVs.
- **Dedupe:** `guid` from measurement timestamp + metric (the CSV has no ids);
  exact strategy confirmed against the sample.

## Build plan

1. **Icebox — do not start without David's go-ahead** (real user demand for
   body-comp trends is the unlock).
2. If built: module `crates/trove-core/src/renpho.rs`, `DEF` (Import, no
   connection), registry line, generic import box handles the file drop.
3. Parser-last: acquire a real Renpho app CSV export first (Needs-sample),
   then write parser + store tests from it, unique temp dirs.
4. Dedupe care: weight rows likely duplicate Apple Health's — the body-mass
   merge rule is the one design decision worth a test of its own.
5. No privacy gate needed per the catalog (weight/body-comp; not in the
   mandatory-flag list).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Body composition | — | export CSV from a real Renpho app; drop into the import box; confirm fat%/muscle/bone/water metric CSVs in `health/` + raw copy in `health/renpho/` |
| Weight dedupe | — | import on a vault that already has the same period's Apple Health export; confirm no duplicate body-mass rows |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Renpho Smart
Scale (L966–L972). Feasibility 🟠 low. Cross-cutting note 8 (L992): body
composition is the systematic Apple-Health blind spot — weight syncs, body
comp doesn't; Withings API and Samsung Health CSV are the *reliable* paths to
body comp, which further weakens the case for a fragile Renpho build.
Renpho-api PyPI package documents the unofficial auth flow if this ever
thaws.
