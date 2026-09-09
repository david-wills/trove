# Ultrahuman

- **id:** `ultrahuman`
- **domains:** `health/` (contract: **document** — per-metric CSV +
  per-source raw, as built; Phase 3 writes the spec page without
  redesigning it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll for new daily/sleep records; watermark
  cursor)
- **connection:** `ultrahuman` — **Personal API Token** (TokenPaste, NOT
  OAuth). The brief said OAuth 2.0 via UltraSignal but the actual published
  developer docs (vision.ultrahuman.com/developer-docs) use a Personal API
  Token with a Bearer `Authorization` header. No OAuth flow, no refresh.
  Developer access is application-gated (proposal review at
  vision.ultrahuman.com). Not shared with other defs.
- **evidence:** official but gated — vision.ultrahuman.com/developer-docs
  (partner.ultrahuman.com API, Personal API Token, documented data list).
  Brief corrected: auth is TokenPaste not OAuth.
- **effort / priority:** L / P2
- **needs:** privacy (CGM glucose is granular health/biometric detail —
  opt-in with explicit acknowledgement) · Needs-login (a ring-wearing
  account to validate) · Needs-David (submit the developer-program
  application; build is blocked on approval)

## What it is

Ultrahuman Ring AIR — smart ring in the Oura segment, growing base. Its
unique value is being the only vendor combining ring biometrics with an
optional CGM patch (the "M1") in one API: recovery + sleep + glucose in a
single request, which has no Apple Health equivalent. Ultrahuman syncs only
basic sleep/activity to Apple Health, so the scores, HRV detail, skin-temp
deviation, and glucose are vendor-API-only.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Daily scores | ring owners | Recovery Score, Sleep Score, Movement Index | official docs |
| Biometrics | ring owners | HRV, resting HR, skin temperature deviation, nightly SpO2 | official docs |
| Glucose (CGM) | only users wearing the optional M1 patch | glucose readings | official docs |

All optional in the contract — a ring-only user's rows simply carry no
glucose; no tier-specific code paths.

## Access & auth

- REST API, OAuth 2.0, per vision.ultrahuman.com/developer-docs. Access
  tokens valid 1 week with refresh — the token manager must refresh
  proactively, and a lapsed refresh surfaces a clear reconnect state, not
  a silent stall.
- Developer program is application-gated; a developer kit loan is offered
  on approval. Until approved this brief is parked — the hub card can ship
  as a `NotWired` "planned" stub.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `health/ultrahuman/raw/YYYY-MM.jsonl` — API response
  objects, full fidelity.
- **Contract layer:** per-metric CSVs per the as-built health shape
  (sleep, HR/HRV, SpO2, temperature, glucose), per the pending Phase 3
  health spec page. Scores land as per-source daily rows like Oura's.
  Overflow in `extra`.
- **Dedupe:** record id (or date + metric for daily rows) as `guid`;
  cursor in `.trove/ultrahuman-sync.json`, rebuildable from output files.

## Build plan

1. **Gate:** submit the UltraSignal developer application (Needs-David);
   record the outcome here. Nothing below schedules until approval.
2. Module `crates/trove-core/src/ultrahuman.rs`: `DEF` (Periodic, a few
   times daily), `CONNECTION` (OAuth, reusing the shared OAuth2 token
   manager from Oura/TickTick — 1-week tokens make refresh handling the
   main test surface), `pull` hook for Sync-now.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from the official docs' documented data list (ring-only AND
   glucose-bearing variants); parser + store + cursor + token-refresh
   tests, unique temp dirs.
5. Privacy gate: ships opt-in (continuous glucose is sensitive biometric
   detail) — explicit acknowledgement on enable.

## Build notes (2026-06-21)

- **Auth corrected:** Brief said OAuth 2.0; actual docs use Personal API Token
  (TokenPaste). User pastes `email:token` (colon-separated); email stored in
  `scope`, token in `access_token` — same pattern as RetroAchievements.
  Email is a required API query param on every request.
- **API shape confirmed:** Endpoint is `GET /api/v1/metrics` (NOT `/partner/daily_metrics`).
  Required params: `email=<account-email>` AND `date=<YYYY-MM-DD>` (single date only —
  no range params). Response wrapper: `{status, data:{metric_data:[{type, object:{values:[{value,timestamp}]}}]}}`.
  Timestamps are epoch **seconds**. Confirmed from mi3nts/ultraHumanAPIReader and
  official developer docs — parser is no longer parked.
- **Day-by-day loop:** The API is single-date-only; the pull loop iterates one
  day per request (no 7-day window batching). OVERLAP_DAYS re-pulls behind the watermark.
- **Intraday time-series:** Each `object.values[]` entry produces one Observation.
  `guid = "<type>:<epoch_seconds>"` — unique per reading, correct for HR (per-minute),
  glucose (~5-min CGM), and once-daily scores alike.
- **Raw layer:** Deduped by `<type>:<date>`; each metric_data entry stored verbatim
  with `_date` tag for month partitioning.
- **Contract:** `reuse-bound` → `health-medical.Observation`. One row per discrete
  reading → `health/medical/ultrahuman/observations/YYYY-MM.jsonl`.
  Raw layer → `health/ultrahuman/raw/YYYY-MM.jsonl`.
- **35 tests pass,** cargo check clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Scores + biometrics | 🧪 built, needs real token | Paste `email:token`; Sync now; confirm rows in `health/ultrahuman/raw/` + `health/medical/ultrahuman/observations/` + hub last-data |
| Glucose (CGM) | 🧪 built, needs M1 user | Same as above — glucose rows appear only when the M1 patch is present; multiple readings per day (one per ~5-min CGM interval) |
| Intraday HR | 🧪 built, needs real token | HR observations have `guid=hr:<epoch_secs>` — one per minute in the ring's measurement window |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Ultrahuman
Ring AIR (L950–L956). Feasibility 🟡 medium (gating, not tech).
Recommendation: build after WHOOP and Withings — smaller base than Oura.
Cross-cutting note #6: reuse the existing trove-core OAuth2 token manager
for all new wearable pulls. Not time-sensitive — API history backfills.
