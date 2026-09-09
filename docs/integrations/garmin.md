# Garmin Connect

- **id:** `garmin`
- **domains:** `health/` (contract: **document** — the as-built per-source
  raw shape; Phase 3 writes the spec page). Workout records route here
  **whole**, embedded GPS routes included — the location view joins them
  at read time (taxonomy rule; the research doc's geo paths predate it).
- **status:** 🧪 built (fixture-tested; bulk-export FIT + CSV import; needs a real export to validate)
- **unavailable_reason:** none
- **behavior:** Import (official bulk-export ZIP is the canonical path;
  ongoing sync is manual re-export, or via Strava for users who auto-sync
  there)
- **connection:** none for the export path. OAuth only if the
  approval-gated official API is ever granted — spike, not a plan.
- **evidence:** official-docs — self-service export at
  garmin.com/account/datamanagement (FIT/GPX/TCX + CSVs, documented ZIP
  layout); `fitparser` Rust crate decodes FIT; official API is
  partnership-gated push OAuth 1.0a
- **effort / priority:** L / P1
- **needs:** privacy (full GPS traces = location trails — opt-in with
  explicit acknowledgement) · time-sensitive (API program status and
  export tooling drift; re-verify in the Phase 4 loop)

## What it is

The dominant GPS sports-watch ecosystem; many users have years (often a
decade+) of activity history here. FIT files are the gold standard for
GPS + biometric activity data: full GPS traces, power-meter data, training
effect, HRV-at-rest, body battery — none of which flow to Apple Health
(Garmin syncs only basic summaries, steps, HR, sleep, SpO2 over).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Activities (FIT) | all accounts | full GPS trace, HR, power, cadence, training effect, all sensor data | official export, fitparser crate |
| Activity summaries (CSV) | all accounts | DI_CONNECT/ CSVs: start time, type, distance, duration, calories, avg HR | official export |
| Health data | all accounts | health summary CSVs incl. body battery, stress, HRV status | official export |
| Courses / workouts | all accounts | planned routes, custom workout files | official export |
| Ongoing API sync | blocked | — | partnership-gated (see below) |

All optional; a ZIP missing a folder just yields fewer streams.

## Access & auth

- **Canonical (M1):** Garmin Connect web → Account Settings → Export Your
  Data → full archive ZIP emailed within 24–48 h. ZIP layout: `Activities/`
  (FIT), `DI_CONNECT/` (summary CSVs), `WorkoutFiles/`, `Courses/`.
  Individual activities also export as GPX/TCX/FIT from the activity page.
- **Official API: not viable standalone.** Business-use-only application
  (garmin.com/…/GarminConnectDeveloperAccess), push-based OAuth 1.0a —
  Garmin pushes to *your server*, which a local-first app doesn't have.
  Some health metrics carry a commercial license fee. Spike the
  application once and document the outcome; do not plan around it.
- **Unofficial scrapers** (garmin-connect-export, GarminDB lineage) reuse
  the web session, are ToS-gray, and have been intermittently blocked by
  TLS fingerprinting — skip.
- No TCC. Standalone-clean (user-initiated file import).

## Vault mapping

- **Raw layer:** `health/garmin/activities/YYYY-MM/…` — decoded FIT
  records as JSONL (GPS route embedded in the workout record, never
  split out), original FIT preserved or referenced per fixture-time call;
  `health/garmin/<summary>.csv|jsonl` for the DI_CONNECT health/summary
  CSVs.
- **Contract layer:** none — `health/` is a document-domain. Embedded
  routes stay in the health record; `location/` gets nothing directly.
- **Dedupe:** activity id (from filename/metadata) as `guid`; re-imports
  of a newer ZIP upsert, never duplicate.

## Build plan

1. Module `crates/trove-core/src/garmin.rs`: `DEF` (Import — registry
   import box), ZIP walker, FIT decode via the `fitparser` crate
   (pure-Rust), CSV parsers for DI_CONNECT.
2. Registration line in `INTEGRATIONS`; no `CONNECTION`.
3. Fixtures: small real FIT files + a miniature export-ZIP layout;
   GarminDB (Python) as a schema reference. Parser tests in unique temp
   dirs. FIT field coverage is the L-effort core — start with GPS/HR/
   power/laps, land the rest additively.
4. Privacy gate: opt-in acknowledgement on import (GPS trails).
5. UI hint: 24–48 h export turnaround → import copy should say "Garmin
   emails the ZIP within two days"; for ongoing sync, point Strava-synced
   users at the Strava integration.

## Build status — 🧪 2026-06-14

Shipped (`garmin.rs`, INDEX #16 — a `Behavior::Import` collector; no auth/API).
Writes the **raw-only `health/` document-domain** at `health/garmin/` → no
binding (health/ is per-source raw, like gaming/). The official API is
partnership-gated push-OAuth (needs your own server) → not viable standalone;
unofficial scrapers are ToS-gray/blocked → skipped. The bulk-export ZIP is the
only path.

- `ImportSpec` accepts the export ZIP (+ a bare `.fit`). **`Activities/*.fit`**
  decode via the pure-Rust **`fitparser`** crate (decoded by the crate, not
  hand-parsed) → `health/garmin/activities/YYYY-MM/<id>.jsonl`: one JSONL line per
  FIT data message, FULL fidelity (file_id/session/lap/record/event/device_info/…
  + **unknown message types preserved generically**). GPS `lat`/`lon` stay **raw
  (semicircles), embedded in the activity — never split to `location/`** (the read
  layer joins). Partition by the activity's start month.
- **`DI_CONNECT/**/*.csv`** (summary + health CSVs: activities, sleep, steps,
  stress, body-battery, HRV) → `health/garmin/<stream>.jsonl` (header keys →
  values; any CSV handled generically).
- `guid` = the activity id (filename stem; fallback `file_id` time+serial);
  **collision-proof filename** (a hash suffix is added only when sanitization
  alters a "dirty" guid — numeric Garmin IDs stay plain `12345.jsonl`); re-import
  upserts, never duplicates. 🔒 default-off opt-in (GPS-trails acknowledgement;
  import copy notes the ~24–48 h export turnaround + points Strava-syncers at
  Strava).

Evidence: FIT = Garmin's FIT SDK (decoded by `fitparser`); the test fixture is
the public canonical FIT-SDK `Activity.fit` (from `fitparser`'s own test data) —
a real decode test, byte-for-byte verified. DI_CONNECT CSVs per the GarminDB
reference.

Adversarial-verify: FIT decode byte-for-byte lossless (22 messages, full GPS
sequence, unknowns preserved, FIT epoch→local correct), guid/dedup correct, GPS
never to `location/`, no network, `newest_mtime_recursive` additive, panic-safety
all confirmed; 0 blocking + 1 minor fixed (a silent filename-collision data-loss
edge → collision-proof filenames).

**Deferred:** `WorkoutFiles/` + `Courses/`, rarer FIT fields (added additively),
original-`.fit` byte retention (the decoded JSONL is the full-fidelity raw layer).

Gate: trove-core 572/0 (+8 garmin tests), `cargo check` clean, `schedule_doc`
regenerated (garmin Import), `bindings.ts` up to date. `health/` document/raw — no
contract changes.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Export ZIP import (FIT + CSVs) | 🧪 (needs a real export) | request a real export at garmin.com/account/datamanagement (~24–48 h email); import the ZIP; confirm `health/garmin/activities/` (FIT) + `health/garmin/<stream>.jsonl` (CSVs) + Recent-data view |
| Re-import dedupe | 🧪 (needs a real export) | import the same ZIP twice + a newer ZIP; confirm no duplicate activities (guid upsert) |
| Official API spike | 🚫 Needs-David | submit the developer-access application once; record the outcome here (not viable standalone — push-OAuth needs a server; low priority) |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Garmin
Connect (L854–L860) + "Geolocation & Travel" §Garmin Connect (L2284–L2290)
and §Garmin Connect Bulk Export (L2404–L2410) — three research entries,
one provider, one brief. Feasibility 🟡 medium overall, 🟢 high for the
export path. June 2026: the official API program FAQ says applications
are open (an earlier suspension report looks outdated) — hence the
time-sensitive flag. Strava is the better ongoing-sync path for the many
Garmin users who auto-sync there; high overlap with Apple Health workout
routes for Apple Watch owners, but this covers everyone else.
