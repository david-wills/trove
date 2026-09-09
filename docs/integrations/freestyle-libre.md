# FreeStyle Libre (LibreView)

- **id:** `freestyle-libre`
- **domains:** `health/` (contract: **document** — the per-metric CSV +
  per-source raw shape already built; Phase 3 writes the spec page without
  redesigning it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (LibreView CSV export — the official, reliable path)
- **connection:** none (CSV import needs no login; the unofficial
  LibreView API would need account credentials — deferred, see notes)
- **evidence:** official LibreView CSV export (libreview.com → Reports →
  export); unofficial API community-documented at
  libreview-unofficial.stoplight.io (fragile / ToS-gray); official Abbott
  API is partner-only
- **effort / priority:** M / P2
- **needs:** privacy (continuous glucose is medical data — opt-in with
  explicit acknowledgement)

## What it is

Abbott's FreeStyle Libre is the other major consumer CGM (market share
roughly even with Dexcom in some demographics); readings flow sensor →
LibreLink app → LibreView cloud. Abbott offers no public developer API, so
the official LibreView CSV export is the dependable path. Supporting both
Libre and Dexcom matters — together they cover essentially the consumer
CGM market.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Glucose history (CSV) | any LibreView account | timestamped glucose readings at scan frequency (every 15 min on Libre 2/3), full history | official export |
| Continuous cloud sync | **not available** — official API is Abbott-partner-only; unofficial API is fragile/ToS-gray | — | community docs (low confidence) |
| Basic glucose via Apple Health | LibreLinkUp users | already captured by the shipped Apple Health import (CoveredBy overlap) | official |

All optional in the contract; no tier-specific code paths.

## Access & auth

- CSV: log in at libreview.com → Reports → export CSV of glucose readings
  (also reachable via clarity.libreview.com per the research doc). User
  drops the file on the import box — no in-app auth, no TCC.
- Unofficial API: HTTPS POST to libreview-us.abbott.com with an app-id
  header + account credentials (documented at
  libreview-unofficial.stoplight.io, used by several open-source
  projects). Reverse-engineered, ToS-gray, breaks without notice — if ever
  built, it ships with an explicit "unofficial" disclosure like Eight
  Sleep. Not in this brief's scope.
- Third-party aggregators (Terra, Thryve, Junction, Tidepool) hold formal
  Abbott partnerships but add a cloud middleman — rejected.
- Standalone-clean as built (file drop only).

## Vault mapping

- **Raw layer:** `health/freestyle-libre/` — parsed readings as
  `egvs/YYYY-MM.jsonl`; original CSVs kept under
  `health/freestyle-libre/imports/`.
- **Contract layer:** glucose joins the per-metric layout as built —
  `health/blood-glucose/YYYY-MM.csv` + `daily.csv` — merging at read time
  with Apple Health and Dexcom rows.
- **Dedupe:** readings key by timestamp (re-imported overlapping exports
  upsert cleanly); no cursor needed for a manual import.

## Build plan

1. Reuse the Dexcom Clarity CSV parser structure (same basic shape:
   timestamped EGVs + events) — build the two as one shared CGM-CSV
   helper with per-vendor column maps.
2. Module `crates/trove-core/src/freestyle_libre.rs`: `DEF`
   (Import behavior, import hook), no `CONNECTION`.
3. Registration line in `INTEGRATIONS`.
4. Fixture: a LibreView CSV sample — the export exists officially but the
   research doc carries no column-level schema, so the parser is written
   against a real file: **Needs-sample at build time** (parser-last if
   none is on hand).
5. Privacy gate: ships opt-in (medical data).
6. UI copy: note that LibreLinkUp users already get basic glucose through
   the Apple Health import — this import adds full-resolution history.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import | 🧪 built; 24 unit tests pass | export a real LibreView CSV, drop on import box, confirm `health/medical/freestyle-libre/observations/YYYY-MM.jsonl` rows + `health/freestyle-libre/raw/YYYY-MM.jsonl` + hub last-data; needs a real Libre wearer |

## Build notes (2026-06-17)

- Behavior: Import (CSV). No connection (no login). Status: 🧪 built.
- Reuses `health-medical` Observation contract (same sink as Dexcom, LOINC 2339-0 for glucose + LOINC 2514-8 for ketone).
- CSV format confirmed from multiple open-source parsers (Tidepool/libreViewDriver.js, shrugalic/LibreView_to_AppleHealth_converter, RaunakMandal/Freestyle-Libre-Viewer, philipp-1337/glucose-data-processor).
- Vault: raw layer unconditional at `health/freestyle-libre/raw/YYYY-MM.jsonl`; contract layer at `health/medical/freestyle-libre/observations/YYYY-MM.jsonl`.
- Unit detection by value inspection (≥40 → mg/dL, <40 → mmol/L) following Tidepool's approach.
- Guid: `{serial_number}|{raw_timestamp}|{record_type}` — stable and collision-free, no server-assigned id in the CSV.
- Re-import is fully idempotent (guid dedupe against existing contract rows).
- Timestamp formats: DD-MM-YYYY HH:MM (24h), MM-DD-YYYY HH:MM (24h), DD-MM-YYYY HH:MM AM/PM, MM-DD-YYYY HH:MM AM/PM.
- 24 tests, all green. No new deps. cargo check clean.

## Research notes

`integrations-research.md` → Health: Wearables & Biometrics §Abbott
FreeStyle Libre / LibreView (L886–L892) and Health: Nutrition/Medical
§Abbott FreeStyle Libre (L1129–L1135). Feasibility 🟡 medium. The
unofficial-API spike stays on the books but is explicitly deferred —
revisit only if users ask for continuous sync; Nightscout (self-hosted
aggregator, trivial official API) is the better answer for power users and
sequences after Dexcom.
