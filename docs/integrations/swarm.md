# Swarm (Foursquare)

- **id:** `swarm`
- **domains:** `location/` (contract: **Phase 3 pending** — location; trails
  shape exists now, a visits-shaped record waits for a visits source — Swarm
  check-ins are the first such source)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll `self/checkins`; watermark on newest check-in
  timestamp)
- **connection:** `swarm` — OAuth (Foursquare/Swarm; user grants access to
  their own check-in history). New connection, not shared.
- **evidence:** community-confirmed — `GET api.foursquare.com/v2/users/self/checkins`
  reported live as of Dec 2025 (blog post on exporting to Day One). API is
  undocumented-ish; medium confidence. GDPR JSON export is the M1 fallback.
- **effort / priority:** M / P2
- **needs:** privacy (check-ins are a location trail — opt-in with explicit
  acknowledgement)

## What it is

Swarm is Foursquare's surviving check-in app: a manual "I'm here" log of
venues the user visits. Foursquare City Guide is dead (app Dec 2024, web
~May 2025); Swarm lives on. Check-ins are a high-signal, user-curated
"places I've been" record — venue name, coordinates, category, timestamp —
that nothing else in the vault captures.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Check-ins | free | venue name, lat/lon, category, timestamp, optional shout/photo | community |

All optional in the contract (omit-if-empty).

## Access & auth

- **API:** `GET https://api.foursquare.com/v2/users/self/checkins` with an
  OAuth token (the user authorizes access to their own history). Returns
  check-in records with venue, coordinates, category, and timestamp.
- **Reliability caveat:** some users report `402` errors; Foursquare
  acknowledged this as a bug and confirmed users should not be charged for
  their own data. On `402`, surface the GDPR-export fallback rather than
  failing silently.
- **Fallback:** GDPR data export at `foursquare.com/download-data` returns
  JSON — the M1 path and a clean parser-reuse target.
- **Standalone-clean:** plain HTTPS OAuth, no TCC, no local app.

## Vault mapping

- **Raw layer:** `location/swarm/raw/YYYY-MM.jsonl` — the API check-in
  objects (or GDPR-export records), full fidelity.
- **Contract layer:** `location/swarm/` per the (pending) **location**
  contract. Check-ins are *visits*, not trails — they seed the
  visits-shaped record the location contract is waiting on (one row per
  check-in: `ts`, `source`, `guid` = check-in id, `lat`, `lon`, `venue`,
  `category`), Swarm extras (shout, photo, sticker) in `extra`. Parked
  behind Needs-David until the location contract ratifies the visits shape.
- **Dedupe:** check-in id as `guid`; cursor in `.trove/swarm-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/swarm.rs`: `DEF` (Periodic),
   `CONNECTION` (`swarm` OAuth: label/help/scope copy on the def per the
   affordance rule), `pull` hook.
2. Register one line each in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the community-documented `self/checkins` response shape AND
   a GDPR-export JSON sample; parser + store + cursor tests, unique temp
   dirs. Cover the `402` → fallback path.
4. Privacy gate: ships opt-in (check-ins are a location trail) — explicit
   acknowledgement on enable.
5. Vault writes via `store` once the location visits shape ratifies; until
   then **parked behind Needs-David (contract)**.

## Build notes (2026-06-21)

- Behavior: Periodic (hourly), OAuth. New CONNECTION `swarm` (redirect port 38845).
- contract_mode: raw-only — location.md spec explicitly states Swarm check-ins are visit-shaped, NOT trail fixes; they do not fit the existing `location.Fix` contract. Raw written unconditionally to `location/swarm/raw/YYYY-MM.jsonl`.
- API: `GET api.foursquare.com/v2/users/self/checkins` with `oauth_token`, `v=20240101`, `limit=250`, `offset`. Response: `response.checkins.items[]`. Watermark: max `createdAt` (Unix seconds); incremental via `afterTimestamp=`.
- 402 bug: Foursquare returns 402 for some accounts (known bug); surfaces descriptive error advising GDPR export fallback.
- Foursquare does NOT issue refresh tokens — expiry = reconnect.
- 11 unit tests, all passing. cargo check clean.
- Integrator: add `&crate::swarm::CONNECTION,` to CONNECTIONS in integrations.rs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Check-ins (API) | ✅ built | complete OAuth in the connect card; Sync now; confirm rows in `location/swarm/raw/` + hub last-data |
| 402 fallback guidance | ✅ built | if API returns 402, pull returns descriptive error message pointing to GDPR export |
| GDPR export | — | not yet an import path; raw format is identical to API output |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Swarm (Foursquare)
(L2356–L2362). Feasibility 🟡 medium. Gotchas: the v2 endpoint is
undocumented-ish, `402` bug exists (acknowledged), and the user base has
shrunk — research recommends building it after higher-value GPS sources,
with the GDPR export as a reliability backstop.
