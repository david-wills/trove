# Suunto

- **id:** `suunto`
- **domains:** `health/` (contract: **document** — per-metric CSV +
  per-source raw, as built; Phase 3 writes the spec page without
  redesigning it). Workout records route here **whole**, embedded GPS
  routes included — the location view joins them at read time.
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (per-activity FIT/GPX from the Suunto app + bulk
  JSON export from suunto.com)
- **connection:** none for the imports. The Suunto Cloud API would need an
  OAuth connection, but approval has a "companies and organizations" bias —
  personal developer access unclear; recorded, not assumed.
- **evidence:** official docs at apizone.suunto.com (Azure API Management)
  but business-use bias, personal access unclear; FIT/GPX export and bulk
  JSON export are the documented user-facing paths
- **effort / priority:** L / P2
- **needs:** privacy (workout GPS routes are location trails — opt-in with
  explicit acknowledgement)

## What it is

GPS sports watches (diving, trail, multisport heritage). Suunto syncs
activity, HR, and sleep to Apple Health, so the already-built Apple Health
import covers the basics for iPhone users; the API-only extras are training
load and swim-specific metrics. Smaller user base than Garmin/Polar —
research recommendation is build later.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Workout FIT/GPX import | none (app exports one at a time) | GPS route, HR, pace, laps | research doc; FIT via `fitparser` |
| Bulk JSON export | none (from suunto.com) | full account workout history | research doc — format undocumented in research, **Needs-sample** |
| Cloud API (workout FIT URLs via webhook/poll, 24/7 activity, sleep) | application approval | workouts, routes, daily activity, sleep | official, gated |

All optional in the contract; an import-only user simply has no daily
activity rows.

## Access & auth

- **Import paths (build these):** per-activity FIT/GPX from the Suunto app;
  bulk JSON export from suunto.com. No auth, no TCC.
- **API path (deferred):** OAuth 2.0 at apizone.suunto.com; the SuuntoPlus
  opening (March 2026) is for watch-face/app developers, **not** the Cloud
  API — data-extraction approval remains a longer process. Webhook FIT-URL
  notifications would be the cleanest path if ever granted, but webhooks
  don't fit a local-first app without a relay — poll instead.
- Standalone-clean either way.

## Vault mapping

- **Raw layer:** `health/suunto/workouts/` — one record per imported
  activity (decoded FIT/GPX → JSON, route embedded), partitioned by month;
  bulk-export JSON lands as `health/suunto/raw/` full-fidelity first.
- **Contract layer:** per-metric CSVs per the as-built health shape where
  metrics exist; workout rows follow the health workout shape the Phase 3
  spec page will document. Overflow in `extra`.
- **Dedupe:** activity start-time + workout id (bulk JSON) or FIT header
  identity as `guid`; bulk export + per-file import of the same workout
  must collapse to one record.

## Build plan

1. Reuse the **shared FIT ingestor** (`fitparser`, built once for
   Garmin/Polar/COROS/Suunto/Wahoo) — no bespoke FIT code here.
2. Module `crates/trove-core/src/suunto.rs`: `DEF` with `Behavior::Import`
   accepting FIT, GPX, and the bulk JSON zip; route by detected structure.
3. Bulk JSON shape is not documented in the research doc — **parser-last,
   Needs-sample**: ship FIT/GPX first, add the bulk parser when a real
   export is in hand.
4. Privacy gate: GPS routes are location trails — opt-in with explicit
   acknowledgement on enable.
5. API application only if user demand appears; record any outcome here.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| FIT/GPX workout import | ✅ built | export an activity from the Suunto app; drop in the import box; confirm record in `health/suunto/workouts/YYYY-MM/` + hub last-data; re-import is a no-op |
| ZIP bulk import (FIT+GPX) | ✅ built | drop suunto.com export ZIP; FIT + GPX entries route to workouts/, other content to raw/ |
| Bulk JSON import | 🔒 parked | bulk JSON format undocumented — raw files preserved in `health/suunto/raw/`; real parser parked until export sample is available (Needs-sample) |
| Cloud API | — | blocked on approval — not scheduled |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Suunto
(L926–L932). Feasibility 🟡 medium. Recommendation: build later — small
base, Apple Health covers most; FIT export is the M1 fallback. Cross-cutting
note #3: Suunto/COROS/Garmin all gate their APIs; FIT is the universal
fallback. Not time-sensitive (exports are backfillable).
