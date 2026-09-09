# Nightscout

- **id:** `nightscout`
- **domains:** `health/` (contract: **document** — per-metric CSV + per-source
  raw, as built; Phase 3 writes the spec page without redesigning it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the user's own Nightscout instance; watermark
  cursor on entry timestamps)
- **connection:** `nightscout` — TokenPaste (the user supplies their own
  Nightscout URL + API secret / JWT token; no OAuth, no approval, no broker).
  Not shared with other defs.
- **evidence:** official-docs — nightscout.github.io (open-source, actively
  maintained; REST API v3; v2 API added "easy state" statistics in 2026)
- **effort / priority:** S / P2
- **needs:** privacy (continuous glucose stream is detailed medical data —
  opt-in with explicit acknowledgement)

## What it is

Nightscout is the self-hosted, open-source CGM aggregator run by the T1D
community: users deploy it themselves (cloud host or local machine) and feed
it from Dexcom Share, Abbott Libre, Medtronic, Eversense, and other CGM
sources. For anyone who runs one, it is a single endpoint for their *entire*
multi-brand glucose history plus treatment log — niche audience, but the
users who have it are exactly the users who will want it in their vault, and
the build is technically trivial.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Glucose entries | none (self-hosted, free) | EGV readings w/ timestamps, device/source | official docs (`/api/v3/entries`) |
| Treatments | none | insulin doses, carb entries, event notes | official docs (`/api/v3/treatments`) |
| Device status | none | uploader/pump/CGM device state snapshots | official docs (`/api/v3/devicestatus`) |

All optional in the contract; a glucose-only instance simply yields no
treatment rows. No tier gating anywhere — Nightscout has no plans.

## Access & auth

- REST against the user's own instance: `GET <nightscout-url>/api/v3/entries`,
  `/api/v3/treatments`, `/api/v3/devicestatus`; auth via API secret or JWT
  token (`?token=` / header). Pagination per the v3 API; research doc's
  sketch: `GET /api/v3/entries?count=10000&token=<token>` with pagination.
- No rate-limit concern — it's the user's own server.
- No TCC, no local files. Standalone-clean: plain HTTPS to a user-supplied
  URL. The instance itself is the user's existing infrastructure, not a
  runtime dependency Trove introduces — Trove only ever *reads* from it and
  degrades gracefully (clear error, no data loss) when it's unreachable.

## Vault mapping

- **Raw layer:** `health/nightscout/raw/YYYY-MM.jsonl` — entries, treatments,
  and devicestatus objects at full fidelity, month-partitioned.
- **Contract layer:** the documented health shape (per-metric CSV, as built):
  glucose rows join the same blood-glucose metric stream the Apple Health
  export writes; treatments (insulin/carbs) stay in the raw layer until the
  Phase 3 spec page says otherwise.
- **Dedupe:** entry `_id` (or `date`+`device` composite) as `guid`; cursor in
  `.trove/nightscout-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/nightscout.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste with **two** fields' worth of setup copy: instance
   URL + token; validate the URL by hitting `/api/v3/status` on connect).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented v3 response shapes (entries, treatments,
   devicestatus); parser + store + cursor + pagination tests, unique temp dirs.
4. Privacy gate: ships opt-in (continuous glucose is detailed medical data) —
   explicit acknowledgement on enable.
5. Sequence **after Dexcom** (per the research doc) so the glucose metric
   conventions are already exercised by the bigger source; reuse them here.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Glucose entries | ✅ built | paste a live Nightscout URL + token; Sync now; confirm rows in `health/medical/nightscout/observations/` + hub last-data |
| Treatments | ✅ built (raw) | same instance with insulin/carb logging enabled; confirm treatment rows in `health/medical/nightscout/raw/treatments/` |
| Device status | ✅ built (raw) | same sync; confirm devicestatus snapshots in `health/medical/nightscout/raw/devicestatus/` |

## Build notes (2026-06-16)

- Follower of the `health-medical` domain contract bound by `dexcom.rs`.
- SGV entries → `Observation` (LOINC `2339-0`, mg/dL), via contract + raw layers. Treatments and devicestatus → raw only (no bound contract for these shapes).
- Connection: `TokenPaste` — composite `https://your-site.com:api-secret`. Port-aware credential parsing handles `http://host:PORT:token` correctly.
- `identifier` (v3 UUID) preferred as guid; falls back to `_id` (MongoDB ObjectId); date-based fallback for pre-v3 entries.
- Pagination: pages of 1000 with `date$gte` watermark per collection; drains fully before advancing cursor.
- Needs a real self-hosted instance to validate end-to-end; David doesn't run one. Any T1D user's run validates.

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Nightscout
(L958–L964). Feasibility 🟢 high — clean REST API, fully open-source, no
approval needed. Aggregates Dexcom/Libre/Medtronic/Eversense, so for a
self-hosting user it can substitute for per-vendor CGM pulls. The Nightscout
Connect plugin (2026) can also import from vendor clouds — that's the user's
instance's concern, not Trove's. Niche-but-trivial: small audience,
technically sophisticated, high appreciation.
