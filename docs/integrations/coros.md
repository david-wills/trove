# COROS

- **id:** `coros`
- **domains:** `health/` (contract: **document** — per-metric CSV + per-source
  raw, as built; Phase 3 writes the spec page without redesigning it).
  Workout records route here **whole**, embedded GPS routes included — the
  location view joins them at read time.
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (per-activity FIT files exported from the COROS app)
- **connection:** none for the FIT import. The COROS Cloud API would need an
  OAuth connection, but access is application-gated (business/partnership
  model) — recorded as a spike, not assumed.
- **evidence:** official but application-gated API
  (support.coros.com/hc/en-us/articles/17085887816340 — API application);
  FIT export is the reliable documented path, decoded by the `fitparser`
  crate
- **effort / priority:** L / P2
- **needs:** privacy (workout GPS routes are location trails — opt-in with
  explicit acknowledgement) · time-sensitive (COROS cloud keeps a recent
  window; live/regular capture beats waiting)

## What it is

GPS sports watches with a growing base among runners and triathletes.
Carries training-analytics fields (training load, running power, detailed
workout telemetry) beyond what COROS pushes to Apple Health. For iPhone
users the already-built Apple Health import captures the basics; this
integration adds full workout fidelity and covers non-iPhone users.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Workout FIT import | none (app exports per activity) | GPS route, HR, pace, power, laps, training metrics | research doc; FIT is a documented binary format (`fitparser`) |
| Activities / daily health / sleep via Cloud API | application approval | activities, training plans, daily health, sleep | official, gated — spike |

All optional in the contract; a user who only imports FIT files simply has
no daily-health rows.

## Access & auth

- **Import path (build this):** user exports `.fit` per activity from the
  COROS app; Trove's generic import box accepts the files. No auth, no TCC.
- **API path (spike):** formal application at support.coros.com; OAuth once
  approved. Not self-service — outcome must be documented before any API
  work is scheduled.
- The COROS MCP server (May 2026) requires the user to keep it running —
  violates the standalone rule for regular sync; at most a labeled opt-in
  M6 later.

## Vault mapping

- **Raw layer:** `health/coros/workouts/` — one record per imported FIT
  activity (decoded JSON, route embedded), partitioned by month.
- **Contract layer:** per-metric CSVs per the as-built health shape (HR,
  sleep, steps as applicable once API data exists); workout rows follow the
  health workout shape the Phase 3 spec page will document. Overflow in
  `extra`.
- **Dedupe:** FIT activity start-time + device serial (from the FIT file
  header) as `guid` — re-importing the same file is a no-op.

## Build plan

1. **Shared FIT ingestor first** — `fitparser` compiled into trove-core;
   one parser serves Garmin, Polar, Suunto, Wahoo, and COROS (research
   cross-cutting note #2). COROS should not get a bespoke FIT path.
2. Module `crates/trove-core/src/coros.rs`: `DEF` with `Behavior::Import`,
   routing FIT files through the shared ingestor and tagging
   `source: coros`.
3. Fixtures: real COROS-exported FIT sample (flag **Needs-sample** if none
   on hand — FIT is documented but vendor field quirks are real).
4. Privacy gate: GPS routes are location trails — opt-in with explicit
   acknowledgement on enable.
5. Separately: submit the COROS API application and record the outcome in
   this brief; only then scope an OAuth Periodic def sharing this module.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| FIT workout import | ✅ built | export an activity as .fit from the COROS app; drop it in the import box; confirm workout record in `health/coros/activities/YYYY-MM/` + hub last-data; re-import is a no-op |
| Cloud API pull | ⛔ blocked | blocked on application approval — document outcome first |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Coros
(L918–L924). Feasibility 🟡 medium (the API); the FIT path is the
dependable one. COROS syncs basic activity/health to Apple Health, so the
incremental value is full workout telemetry. Time-sensitive per the
research doc's retention notes (L153): Garmin/COROS recent windows.
Sequence after the FIT ingestor exists (Garmin justifies building it).
