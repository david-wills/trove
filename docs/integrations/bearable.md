# Bearable

- **id:** `bearable`
- **domains:** `health/` (contract: **document** — per-source raw under
  `health/<source>/`, alongside the existing per-metric CSV layout)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (drop the app's CSV export into the vault)
- **connection:** none (manual in-app export; no API)
- **evidence:** community-documented CSV format (GitHub:
  `samstarling/bearable-csv`); 900K+ users. No public API; auto-export on
  roadmap but not shipped. **Needs-sample** to confirm column set across app
  versions.
- **effort / priority:** S / P2
- **needs:** privacy (mood/symptom/medication detail = health detail-class —
  opt-in with explicit acknowledgement) · Needs-sample (real CSV to confirm
  columns)

## What it is

Symptom/mood/medication tracker. The user logs mood ratings, pain/fatigue
scores, symptoms, medications taken, and lifestyle factors per entry with
timestamps. Fills the manual self-tracking niche no other structured source
covers — especially valuable for chronic-condition and medication-adherence
tracking, complementing EHR-sourced prescriptions with actual adherence and
symptom-response data.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Mood & symptom log | free tier (basic export) | mood ratings, pain/fatigue/symptom scores, timestamps | community schema |
| Medication adherence | free tier | medications taken, custom factors | community schema |
| Lifestyle factors | free tier | sleep hours, steps, custom factors | community schema |
| Full history export | premium ($34.99/yr) | full-history CSV vs. recent-only on free | community schema |

All optional; rows carry whatever the user tracked. No tier-specific code path —
a free-tier export simply spans a shorter window.

## Access & auth

- Manual export only: Bearable app → Settings → Export Data → CSV (emailed/saved
  on device), then the user drops the CSV into Trove. No API, no OAuth, no TCC,
  no local DB on the Mac.
- Standalone-clean (file hand-off). Scheduled auto-export is on Bearable's
  roadmap but not available, so this stays an Import behavior.

## Vault mapping

- **Raw layer:** `health/bearable/raw/<export>.csv` plus a parsed
  `health/bearable/YYYY-MM.jsonl` of per-entry rows (full fidelity — every
  factor column preserved). `health/` is a document domain (raw-only per
  source), so Bearable keeps its own native shape; it is not forced into a
  cross-source contract.
- **Contract layer:** none — these are subjective self-tracked factors, not the
  quantitative per-metric biometric CSVs that `health/` builds for wearables;
  they stay in the per-source folder. Read-time views can surface mood/symptom
  series alongside biometrics by timestamp.
- **Dedupe:** per-entry `guid` from (entry timestamp + factor name + value);
  re-importing an overlapping export merges without duplicating rows.

## Build plan

1. Module `crates/trove-core/src/bearable.rs`: `DEF` (Import), CSV parser
   handling the long-format factor rows, store raw CSV + parsed JSONL via `store`
   helpers.
2. Registration line in `INTEGRATIONS`. No connection.
3. **Needs-sample:** the column set/headers vary across app versions and the
   schema is community-documented, not official — flag Needs-sample and validate
   against a real export; parser tolerant of unknown factor columns (carry them
   through verbatim).
4. Privacy gate: ships opt-in — explicit acknowledgement (health/symptom detail).
5. Fixtures from the `samstarling/bearable-csv` documented shape; parser + store
   + dedupe tests, unique temp dirs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import | ✅ built | 7 unit tests pass; `bearable::tests::parses_all_rows_and_maps_fields_correctly` covers all factor categories |
| Column coverage | ✅ built | fixture covers Mood/Symptom/Medication/Sleep/Steps; positional parser is tolerant of unknown columns (extra fields beyond index 6 are silently ignored) |
| Dedup / re-import | ✅ built | `reimport_is_idempotent_no_duplicates` — guid from date|time_of_day|category|detail |
| Hub card + last_data | ✅ built | `hub_card_and_last_data_surface` test confirms import box and last_data via registry |
| Real export validation | ⚠️ Needs-sample | column set/headers confirmed against samstarling/bearable-csv types.ts but no real app export on disk |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Bearable (L1153–L1159). Feasibility 🟢 high. No public API — manual
export is the only path. Complements pharmacy/EHR `MedicationRequest` data
(adherence + symptom response that prescriptions alone can't show). Premium
unlocks full history; free tier exports a recent window — same parser either way.
