# Streaks

- **id:** `streaks`
- **domains:** `habits/` (catalogued; nothing written — see status)
- **status:** 🚫 unavailable
- **unavailable_reason:** Streaks has no API and no documented export; its
  iCloud sync container is opaque and undocumented. Workout-linked habits may
  still surface through the Apple Health export.
- **behavior:** Unavailable (never toggleable, never default_on)
- **connection:** none
- **evidence:** community/folklore — no API or export documented; iCloud sync
  container schema unknown (low confidence)
- **effort / priority:** M / P2
- **needs:** none

## What it is

Popular iOS-primary habit tracker (Streaks by Crunchy Bagel, App Store id
963034692) with a Catalyst Mac build. Habits sync via iCloud. It is squarely
in scope for `habits/` by shape — daily yes/no/skip habit checkmarks — but
offers no path to its data: no public API, no advertised export, and an
undocumented iCloud container.

## Capabilities (what data it can yield)

None reachable. The habit data exists only inside an opaque iCloud sync
container. HealthKit-integrated (workout-linked) habits are the sole indirect
signal, and those arrive via the separate Apple Health export path, not from
Streaks itself.

## Access & auth

No API, no documented export, no readable local DB path. The Catalyst Mac build
may place an iCloud container on disk, but its schema is undocumented and would
require reverse-engineering with no stability guarantee — out of scope under
the standalone/honest-access rules. Manual screenshots are the only user-facing
"export," which Trove does not ingest.

## Vault mapping

None — nothing is written. If a documented export or API ever appears, the
target is the `habits/` domain (Phase 3 contract pending, shared with Habitica,
TickTick habits, Way of Life).

## Build plan

No build. Render the honest unavailable card (greyed + reason) from
`Behavior::Unavailable`. Revisit only if Streaks ships an export or API.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| — | 🚫 | n/a — unavailable card; nothing to validate |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity" §Streaks
(Habit Tracker) (L2698–L2704). Feasibility 🟠 low; research recommendation is
icebox. Note the lookalike: a separate "Streaks 2026" listing (id 6740426283)
is an unrelated app — the mainstream app is Crunchy Bagel's (id 963034692).
Workout-linked habits may surface through Apple Health rather than here.
