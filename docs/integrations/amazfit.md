# Amazfit (Zepp)

- **id:** `amazfit`
- **domains:** `health/` (contract: **document** — per-metric CSV +
  per-source raw, as built; Phase 3 writes the spec page without
  redesigning it). Workout records route here **whole**, embedded GPS
  routes included — the location view joins them at read time.
- **status:** 📋 queued (iceboxed — Needs-David: build only on real demand)
- **unavailable_reason:** none
- **behavior:** Import (per-workout GPX exported from the Zepp app — the
  only stable path)
- **connection:** none. The reverse-engineered cloud path would need
  account credentials against api-mifit.huami.com; deliberately not
  planned (fragile, regional endpoint variants, no stability guarantees).
- **evidence:** low — no official API; community-documented unofficial
  endpoints (haid.app project, `api-mifit.huami.com`
  `/v1/sport/run/history.json` with apptoken header); GPX export is the
  only reliable documented path
- **effort / priority:** XL / P2
- **needs:** privacy (workout GPS routes are location trails — opt-in with
  explicit acknowledgement) · Needs-David (icebox — build only on real
  demand)

## What it is

Amazfit smartwatches (Zepp Health, Xiaomi ecosystem) — large budget-watch
user base. On iOS the Zepp app already syncs steps, HR, sleep, and SpO2 to
Apple Health, so the already-built Apple Health import covers most of the
data for iPhone users. The only thing Apple Health misses that's reliably
extractable is per-workout GPS, via one-at-a-time GPX export.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Workout GPX import | none (Zepp app exports per workout) | GPS route, timestamps, basic workout stats | research doc; GPX is a standard format |
| Steps/HR/sleep/SpO2 | n/a here | covered by Apple Health import | research doc |
| Unofficial cloud history | account credentials | sport history JSON | community-doc'd, fragile — **not planned** |

## Access & auth

- **Import path (the whole plan):** user exports a workout as GPX from the
  Zepp app; Trove's generic import box accepts it. No auth, no TCC,
  standalone-clean.
- **Cloud path (rejected for now):** `api-mifit.huami.com` (regional
  variants) with an apptoken header; endpoints churn, no Rust library, and
  Apple Health sync already covers the non-GPS data — the cost/benefit
  fails. The Zepp OS developer platform (developer.zepp.com) is for watch
  apps, not data extraction.

## Vault mapping

- **Raw layer:** `health/amazfit/workouts/` — one record per imported GPX
  workout (route embedded), partitioned by month.
- **Contract layer:** workout rows follow the health workout shape the
  Phase 3 spec page will document; non-workout metrics arrive via the
  Apple Health import under its own source, not here. Overflow in `extra`.
- **Dedupe:** workout start-time + track identity from the GPX as `guid`;
  re-import is a no-op. Read-time views should expect overlap with Apple
  Health workout summaries (same workout, two sources) — raw stays
  complete, joining is a read-time concern.

## Build plan

Iceboxed — do not schedule until real user demand surfaces (Needs-David).
When it does:

1. Module `crates/trove-core/src/amazfit.rs`: `DEF` with
   `Behavior::Import` accepting GPX, reusing the shared GPX route parser
   (also wanted by `location/` standalone-GPX import — build it once).
2. Fixtures from a real Zepp-app GPX export — **Needs-sample**; Zepp's GPX
   dialect quirks are unverified, so the parser is written last, against
   the sample.
3. Privacy gate: GPS routes are location trails — opt-in with explicit
   acknowledgement on enable.
4. Revisit the unofficial cloud API only if the haid.app community path
   stabilizes; it would ship like Eight Sleep's (explicit "unofficial"
   disclosure, graceful failure).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| GPX workout import | — | export a workout from the Zepp app as GPX; drop in the import box; confirm record in `health/amazfit/` + hub last-data; re-import is a no-op |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Amazfit /
Zepp Health (L942–L948). Feasibility 🟠 low — hence the icebox.
Recommendation verbatim: Apple Health import already covers Amazfit users
on iPhone; the unofficial API is fragile; build only if user demand is
significant. The XL effort is the cloud path; the GPX-only slice above is
much smaller, which is the argument for shipping just that slice if demand
appears.
