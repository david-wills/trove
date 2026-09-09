# Medisafe

- **id:** `medisafe`
- **domains:** `health/medical/` (contract: **Phase 3 pending** — the
  health/medical contract is FHIR-shaped, drafted from the SMART-on-FHIR
  cluster; Medisafe adherence rows land per-source raw under
  `health/medical/medisafe/`, which is always allowed alongside contract rows)
- **status:** 🧪 built (raw scaffold; parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (user emails themselves the in-app CSV export, drops
  it into Trove)
- **connection:** none
- **evidence:** official in-app CSV export (Reports → Export), **Premium-only
  since the Jan 2026 paywall**; CSV schema not publicly documented —
  sample-required. No API.
- **effort / priority:** S / P2
- **needs:** privacy (medication names, doses, adherence — medical detail;
  ships opt-in with explicit acknowledgement) · Needs-login (a Premium
  Medisafe subscription is required to export at all) · Needs-sample (CSV
  schema undocumented — parser built last, against a real export)

## What it is

Medication-reminder app; its export carries what almost nothing else does:
**adherence** — doses taken vs. missed over time, with notes. Prescription
*facts* are better sourced from FHIR `MedicationRequest` (Epic pull) or
Medicare Blue Button Part D; Medisafe adds the behavioral layer on top.
Paywalled since January 2026 (free users cannot export), which caps the
audience and is why the research doc says build later.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Medication adherence CSV | Premium only (no free export since Jan 2026) | adherence rates, doses taken/missed, timestamps, notes, per-medication/timeframe | research doc L1189 |

All capability fields are optional in the contract (omit-if-empty); the
Premium gate never needs special code paths — free users simply have no file
to import, and the card copy says so (disabled-affordance rule).

## Access & auth

- Export: in-app Reports → Export → choose medication/timeframe → CSV arrives
  by email. Premium subscription required. No API, no OAuth, no token.
- No TCC, no local files, no connection def. Pure Import via the
  registry-driven import box.
- Standalone-clean: nothing runs or polls.

## Vault mapping

- **Raw layer:** `health/medical/medisafe/raw/` — the CSVs as exported, full
  fidelity.
- **Contract layer:** the Phase 3 health/medical contract is FHIR-shaped
  (MedicationRequest etc.) and adherence events don't map onto it cleanly —
  Medisafe rows stay per-source raw at `health/medical/medisafe/` (one row
  per dose event: `ts`, medication, dose, taken/missed, note in `extra`).
  Revisit joining a contract if Phase 3 drafts a medication-adherence shape
  (Bearable's medication rows are the natural second source).
- **Dedupe:** `guid` = content hash of (ts, medication, dose, status) unless
  the CSV carries a stable id — confirmed against the sample.

## Build plan

1. **Parser-last.** CSV schema is undocumented — acquire a real Premium
   export before writing the parser (flag stays **Needs-sample**; David
   doesn't necessarily have a Medisafe Premium account, so any real user's
   export can unblock).
2. Module `crates/trove-core/src/medisafe.rs`: `DEF` with Import behavior; no
   `CONNECTION`. One registration line in `INTEGRATIONS`.
3. **Privacy gate:** medication data is medical detail — ships opt-in with
   explicit acknowledgement on enable, per the privacy-sensitive needs-flag
   rule.
4. Fixtures from the (redacted) sample; parser + store tests, unique temp
   dirs; per-medication and per-timeframe export variants both covered.
5. Card copy: note the Premium requirement up front (disabled-affordance
   lesson — tell the user what unlocks the import), and point free/ex-users
   at the FHIR `MedicationRequest` path (Epic MyChart brief) for prescription
   facts.

## Build status (fan-out)

- **Status:** 🧪 built (raw scaffold; parser parked)
- **contract_mode:** `deferred-sibling-draft` → `health-medical.medication`
- **Vault layout:** `health/medical/medisafe/raw/YYYY-MM.jsonl` (verbatim CSV rows as JSON objects, month-partitioned, re-runnable dedup via `_guid`)
- **Parser:** scaffold only — CSV schema undocumented, no sample on disk. The raw importer accepts any CSV and archives verbatim rows. Activate the normalized adherence-event parser once a real Premium export sample is available.
- **8 tests pass**, `cargo check` green.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Adherence CSV import (raw) | 🧪 built | export from a Premium Medisafe account (Reports → Export), drop into the import box; confirm verbatim rows under `health/medical/medisafe/raw/` + hub last-data |
| Adherence CSV normalized parser | parked | acquire a real Premium export, confirm column names, implement `entry_from_row`, move to `health/medical/medisafe/adherence/YYYY-MM.jsonl` |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Medisafe (L1185–L1191); at-a-glance L1023 (feasibility 🟡 Medium).
Paywall landed Jan 2026; many users are reportedly migrating away, so demand
may stay thin. Coverage strategy from the research doc: FHIR
`MedicationRequest` (Epic/smart-on-fhir briefs) + Bearable symptom/medication
tracking cover most medication needs; Medisafe is the adherence specialist
for Premium holdouts.
