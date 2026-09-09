# Dexcom

- **id:** `dexcom`
- **domains:** `health/medical/` — **first-in-domain collector; this build binds
  the `health-medical.observation` contract** (the `Observation` Rust type in the
  new `health_medical` module + a single `health-medical` DOMAINS entry in
  `contracts.rs`; the `health-medical.observation` fixture is promoted from a
  Phase-3 draft into the ratified round-trip suite in `spec_validation`). The
  sibling `health-medical.medication` / `health-medical.condition` shapes stay
  Phase-3 drafts until a collector writes them (epic-mychart / a FHIR pull /
  Part-D claims follow this shape). Each Dexcom EGV is written as a glucose
  observation under `health/medical/dexcom/observations/YYYY-MM.jsonl`.
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-login + Needs-David (app registration)**
- **unavailable_reason:** none
- **behavior:** `Behavior::Periodic` — hourly (`DEXCOM_SYNC_SECS = 3600`),
  every-on-run cadence (the timer only advances when it actually runs, so
  re-enabling fires immediately). A windowed backfill over Dexcom API v3:
  `/dataRange` gives the account's EGV span, then `/egvs` is walked forward from
  the persisted watermark in ≤30-day windows; the first sync backfills all
  available history, later syncs are incremental. **Scope note vs the brief:** the
  Clarity-CSV import path and the events/calibrations/alerts/devices streams in
  the original plan are NOT in this build — it ships the EGV → observation pull
  only; those remain follow-ups (no contract change needed for events).
- **connection:** `dexcom` — OAuth 2.0 (confidential client; client id/secret in
  the token-form body, not Basic auth; `offline_access` scope → a refresh token,
  so expiry refreshes silently). **A single-use login, NOT the shared Google
  login.** New unique fixed redirect port **38579** (`http://localhost:38579/callback`;
  38573–38578 are taken). App credentials resolve explicit → saved → compiled-in
  (`TROVE_DEXCOM_CLIENT_ID` / `TROVE_DEXCOM_CLIENT_SECRET`, empty baked default —
  so app registration is a Needs-David flag). Token stored 0600 at
  `.trove/sync/dexcom`; never in the rebuildable cursor.
- **evidence:** official-docs — developer.dexcom.com API v3 (OpenAPI 3.0.3
  spec, sandbox available); the EGV + dataRange fixtures are the documented v3
  OpenAPI shapes. **Opus re-confirmed the v3 `/egvs` + `/dataRange` shapes and the
  "all glucose values reported in mg/dL" invariant from primary docs before parse.**
- **effort / priority:** M / P1
- **needs:** **Needs-login** (a Dexcom OAuth login) + **Needs-David** (a
  developer.dexcom.com app registration — no app credentials are baked in) for
  live validation; continuous glucose is medical data, so the def ships **opt-in
  with explicit acknowledgement** (`default_on: false`).

## What it is

Dexcom makes the dominant continuous glucose monitors (G6, G7, ONE, ONE+):
a sensor reading blood glucose every 5 minutes, ~288 readings/day. The full
EGV stream plus the user's logged events (carbs, insulin, exercise) are
transformative for metabolic-health correlation against Trove's food,
sleep, and activity data — and only the current reading reaches Apple
Health, so a direct integration adds real depth.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| EGV stream (5-min glucose) | any Dexcom account | value, trend, timestamp (`/v3/users/self/egvs`) | official docs |
| Events | any | carbs, insulin, exercise, health events (`/v3/users/self/events`) | official docs |
| Calibrations / alerts / devices | any | calibration history, alert log, device list | official docs |
| Data range | any | account-wide first/last data timestamps (drives backfill) | official docs |
| Clarity CSV export | any (manual) | all EGVs + events, 30+ days per export | official docs |

All optional in the contract. Stelo (Dexcom's OTC CGM) does **not** get the
same API access as G6/G7 as of 2026 — Stelo users fall back to the CSV
import, never a special code path.

## Access & auth

- OAuth 2.0 at developer.dexcom.com; REST v3 (`/v3/users/self/egvs`,
  `/events`, `/calibrations`, `/alerts`, `/devices`, `/dataRange`). v2
  endpoints shut down May 2026 — build v3 only.
- Sandbox available for development; fixtures can come straight from the
  OpenAPI examples.
- Self-service access is capped at **5 authorized users** per app — fine
  for a personal pull, a known scale problem for a distributed app (full
  commercial access requires applying to Dexcom Strategic Partnerships;
  production data access involves a HIPAA authorization flow). BYO app
  credentials mitigate the cap; record this in the connect-card copy.
- Clarity path: clarity.dexcom.com → Export icon → date range → CSV. No
  auth in-app; the user drops the file on the import box.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping (as built)

- **Contract layer:** each EGV → a [`health-medical.observation`] row at
  `health/medical/dexcom/observations/YYYY-MM.jsonl` (month = the reading's
  `systemTime` rendered local). `guid` = the EGV `recordId` (the dedupe key);
  `test` = `"Glucose"`, `code` = LOINC `2339-0`, `code_system` = `"loinc"`,
  `value` = the integer mg/dL reading, `unit` = `"mg/dL"`. CGM-specific signal
  (`trend`, `trendRate`, `status`, `displayTime`, device ids) rides under `extra`
  — no contract column is invented for it. The v3 "all values in mg/dL" invariant
  is enforced: a raw `unit` of `"unknown"`/`"mmol/L"` is normalized to mg/dL and
  the raw string is preserved under `extra.rawUnit`.
- **Raw layer:** the verbatim EGV object under
  `health/medical/dexcom/raw/YYYY-MM.jsonl` (full fidelity — fields the contract
  drops, e.g. `transmitterTicks`, survive), partitioned by the same month.
- **Dedupe / cursor:** observations key by `recordId`; the watermark (max
  `systemTime` ingested, as naive UTC) lives in `.trove/dexcom-sync.json` — a
  non-secret, rebuildable file (deleting it just re-walks history), advanced only
  after each window's write so a crash re-drains rather than skips.
- **Note vs the Phase-2 brief:** the old plan wrote a per-metric
  `health/blood-glucose/*.csv`; the build instead binds the unified
  **`health-medical`** domain (one observation stream that a Quest fasting
  glucose or an Epic bundle also lands in), so cross-source glucose lines up on
  one axis at read time. The unbound raw `health/<metric>/` writers
  (garmin/oura/apple-health) are untouched.

## Build plan

1. Parser-first on the Clarity CSV (official, free, no approval) — this is
   the M1 path every Dexcom user can use today.
2. Module `crates/trove-core/src/dexcom.rs`: `DEF` (Periodic + import hook),
   `CONNECTION` (OAuth: baked + BYO creds per ConnectSpec), pull hook
   walking `/dataRange` backward for backfill, Oura-style budgeted windows.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from the OpenAPI 3.0.3 examples + sandbox responses; CSV
   fixture from a Clarity sample; tests with unique temp dirs.
5. Privacy gate: ships opt-in (medical data) — explicit acknowledgement on
   enable.

## Validation matrix

Fixture-tested green (the mapping, the windowed backfill/watermark, dedupe, the
0600-token + cursor-has-no-secret guarantees, the connection's fresh port, and
the `health-medical.observation` schema↔Rust↔doc round-trip all pass in
`cargo test -p trove-core`). Live validation is **blocked on a Dexcom developer
app + a real Dexcom wearer's account** — neither is baked in.

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Mapping + backfill + dedupe + secret-hygiene | ✅ fixture-tested | `cargo test -p trove-core --lib dexcom::` (18 tests) + the `health_medical::` round-trip tests + `spec_validation` (`check::<Observation>` + required-list + doc-verbatim). |
| **Needs-David: register a Dexcom app** | ⬜ blocked | Sign in at **developer.dexcom.com**, create an app, set its OAuth **redirect URI to `http://localhost:38579/callback`** (exact match required). Copy the app's **Client ID + Client Secret**. (Self-service is capped at 5 users — plenty for one account.) |
| API sync (EGVs → observations) | ⬜ blocked (needs the app above **+ a real Dexcom wearer** — David is not one; any real user's run validates) | In Integrations → Dexcom's connect card, paste the **Client ID + Client Secret** (saved, so future connects are just a login), click **Connect** → complete the Dexcom OAuth in the browser. Then **Sync now**. Confirm: (1) rows in `health/medical/dexcom/observations/YYYY-MM.jsonl` with `"test":"Glucose"`, `"code":"2339-0"`, integer `"value"`, `"unit":"mg/dL"`; (2) verbatim EGV objects in `health/medical/dexcom/raw/YYYY-MM.jsonl`; (3) a watermark in `.trove/dexcom-sync.json` **with no token in it**; (4) the hub card's last-data timestamp advances; (5) a second **Sync now** writes 0 new rows (idempotent). |

## Research notes

`integrations-research.md` → Health: Wearables & Biometrics §Dexcom CGM
(L870–L876) and Health: Nutrition/Medical §Dexcom CGM (L1121–L1127).
Feasibility 🟢/🟡. The two entries disagree slightly on API openness — the
wearables entry confirms self-service up to 5 users; the labs entry frames
production as partnership-gated. Resolve in Phase 4 against live docs;
ship CSV-first either way. pydexcom is a useful auth-flow reference.
Abbott FreeStyle Libre is the sibling CGM (`freestyle-libre.md`) — its CSV
import should reuse this parser's structure; Nightscout aggregates both
for self-hosting users and sequences after this brief.
