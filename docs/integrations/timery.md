# Timery

- **id:** `timery`
- **domains:** `time-entries/` (contract: **Phase 3 pending** — time-entries;
  written by the `toggl-track` provider, not by this one)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** CoveredBy(`toggl-track`) — built; never toggleable, never
  default_on; DEF updated from NotWired stub to CoveredBy("toggl-track")
- **connection:** none (the authoritative pull uses the `toggl-track`
  connection)
- **evidence:** Timery is a frontend for Toggl Track with no independent data
  store — high confidence. All records live in Toggl.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Timery is a popular Apple-platform frontend (iOS/macOS) for Toggl Track. It
adds a nicer UI, saved timers, and Shortcuts integration, but stores no
authoritative data of its own — every time entry it shows lives in Toggl
Track. Timery caches Toggl data in iCloud for cross-device sync, but the
system of record is Toggl.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Time entries | (all live in Toggl) | start/stop, project, tags, description — via Toggl | research |

There is no Timery-specific data to capture; the table above describes what
the `toggl-track` provider yields on Timery's behalf.

## Access & auth

No separate access path. Toggl Track is the authoritative store: pull via the
Toggl Track API (the `toggl-track` connection) or, on M3, a local Toggl
SQLite read. The Timery iCloud cache is a derived copy, not a source Trove
should read.

## Vault mapping

- **Raw + contract layers:** owned by the `toggl-track` provider under
  `time-entries/toggl-track/` (Phase-3 time-entries contract — user-asserted
  entries only). Timery writes nothing of its own; it is documented here as
  a CoveredBy alias so the hub explains why there is no separate Timery
  collector.

## Build plan

Do not build. Ship as a `CoveredBy(toggl-track)` catalog entry so the hub
card points users to the Toggl Track integration. The only work is the
one-line registry stub; no module, connection, fixtures, or parser.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Time entries | — | validated transitively: once `toggl-track` is built and validated, Timery users' entries appear under `time-entries/toggl-track/` |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Timery (L2722–L2728). Feasibility 🟢 high (because Toggl carries it). The
research recommendation is explicit: skip — no independent data store;
covered by the Toggl Track connector. Build Toggl Track; document Timery.
