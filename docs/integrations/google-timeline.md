# Google Timeline

- **id:** `google-timeline`
- **domains:** `location/` — **first-in-domain collector; this build binds the
  `location/` contract** (the `Fix` Rust type + the `location` DOMAINS entry in
  `contracts.rs`; the `location.fix` fixture is promoted to the ratified set in
  `spec_validation`). owntracks / overland / smartcar / arc-timeline / etc.
  follow this shape. Semantic place-visits still wait for a visits-shaped
  contract.
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-sample** (the
  contract-layer trail mapping is parked; the import scaffold + raw preservation
  are live)
- **unavailable_reason:** none
- **behavior:** `Behavior::Import(&IMPORT)` — no API, no network, no connection;
  the user exports from the Google Maps app and drops the file into the generic
  import box (`accepts: ["json", "zip"]`; `letterboxd.rs` is the reference
  shape). Re-runnable: re-dropping the same export is idempotent (raw artifact
  keyed by format + content hash).
- **connection:** none — no API. The user manually exports from the Google
  Maps app on their device; legacy Takeout exports also accepted.
- **evidence:** community-schema — locationhistoryformat.com (documents both
  the legacy Takeout format and the new on-device format); the Timelinize /
  Dawarich open-source projects are reference parsers. **Confidence: medium** —
  no official format doc, iOS export reportedly less complete than Android. Per
  the evidence rule (the raindrop `_id` lesson), the build does **not** ship a
  parser against an unverified shape: the contract is bound and the scaffold is
  live, but `map_fixes` (the trail-row mapping) is **parked** until a real
  per-platform `Timeline.json` is on disk.
- **effort / priority:** M / P1
- **needs:** privacy (continuous GPS trail — opt-in with explicit
  acknowledgement; ships off by default) · **Needs-sample** — a real on-device
  `Timeline.json` (ideally one Android + one iOS) and/or a legacy Takeout
  location-history file, to verify exact nested field names/units before
  `map_fixes` is unparked. **No Needs-login / no app registration.**

## What it is

Google's continuous location-history trail — the place visits and movement
segments Google Maps records as you go about your day. Among the most
sensitive and most valuable life-logging data a vault can hold: semantic
place-visits (name, coordinates, duration, activity type) plus the raw GPS
path. Used by anyone who left Maps Timeline enabled.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Place visits | free | place name, lat/lng, start/end, activity type | community schema |
| Activity segments | free | mode (walk/drive/…), distance, timelinePath points | community schema |
| Raw GPS signals | free (on-device export) | `rawSignals` position points | community schema |

All optional in the contract (omit-if-empty); an iOS export may lack
`semanticSegments` entirely and still parse.

## Access & auth

- **On-device export:** Google Maps app → Profile → Your Timeline → Settings
  → Export Timeline data → produces `Timeline.json` on the device, which the
  user drops into Trove's import box.
- **Legacy Takeout:** Google moved location history to on-device storage in
  2024–2025; old Takeout exports still exist in the wild and must parse too.
- No auth, no API, no network. Pure file import. Standalone-clean.
- **Format gotcha:** point coordinates are *strings* like
  `'50.0506312°, 14.3439906°'`, not numbers — the parser must split and
  parse them.

## Vault mapping

- **Raw layer (live, unconditional, full fidelity):**
  `location/google-timeline/raw/<format>-<hash>.json` — the whole imported
  payload preserved verbatim, one file per import named by **detected format +
  FNV-1a content hash** (so re-dropping the same export is idempotent). `<format>`
  is `on-device` / `legacy-semantic` / `legacy-records`. Nothing exported is ever
  dropped.
- **Contract layer (bound; parser PARKED — Needs-sample):**
  `location/google-timeline/YYYY-MM-DD.jsonl` of `crate::location::Fix` rows
  (the `location` contract is **day**-partitioned). When `map_fixes` is unparked,
  activity/movement segments map to **fixes sharing a `trail`** (path points →
  lat/lon parsed from the string/E7 forms, `ts` → local, transport class verbatim
  in `mode`, a stable per-fix `guid`, `distanceMeters`/confidence → `extra`).
  Today `map_fixes` returns empty (no fabricated rows). Semantic **place-visits**
  are parked until a visits-shaped contract lands (don't force them into trails). Activity
  type / place name / confidence overflow to `extra`.
- **Dedupe:** segment/visit `guid` derived from start-time + place id (or a
  content hash where ids are absent); no cursor — import is idempotent on
  re-drop by guid.

## Build plan — DONE (2026-06-15, INDEX #76)

1. ✅ Module `crates/trove-core/src/google_timeline.rs`: `DEF`
   (`Behavior::Import`, default-off, `last_data` from the newest day stem), an
   `ImportSpec` (`accepts: ["json", "zip"]`, no params) fed by the generic
   import box (no Tauri command, no hub wiring — the `letterboxd.rs` shape).
2. ✅ Registration: `&crate::google_timeline::DEF` already in `INTEGRATIONS`
   from the Phase 2 stub; `pub mod google_timeline` unchanged; the registry
   projection flips **Not-wired → Import** in `docs/integration-schedule.md`
   (regenerated). No connection (none to add).
3. ✅ **First-in-domain contract bind:** `crate::location::Fix` + the `location`
   DOMAINS entry in `contracts.rs` (the only **day**-partitioned contract) + the
   `location.fix` fixture promoted to the ratified set in `spec_validation`
   (`spec_validation` 5/5 green; `Fix` round-trips the three-line fixture).
4. ✅ Import scaffold, live and fixture-green: **format detection** of all three
   documented shapes (on-device `semanticSegments`, legacy `timelineObjects`,
   legacy `locations`), **raw preservation** (whole payload verbatim under
   `location/google-timeline/raw/<format>-<hash>.json`), **idempotent re-drop**
   (content-hash filename), **Takeout `.zip` extraction** (scans for the
   location-history JSON), and clean rejection of non-location / invalid JSON.
   11 unit tests, unique temp dirs.
5. ⏸ **Contract-layer mapping PARKED (Needs-sample).** `map_fixes` is the single
   seam that turns segments → `Fix` rows; it returns an empty set today so the
   binding + scaffold stay green without fabricating rows against an unverified
   shape (the evidence rule — raindrop `_id` lesson). The documented per-format
   mapping target is written out in the `map_fixes` doc-comment for the agent who
   unparks it. The import is honest meanwhile: it archives losslessly and the
   headline says trail mapping awaits a sample.
6. ✅ Privacy gate: ships **opt-in**, `default_on: false` (a continuous
   where-you've-been trail is first-class privacy-sensitive).
7. ⏸ **Place-visits parked behind the visits contract:** semantic place-visits
   stay raw, never forced into a `Fix` row (the location-domain ruling).

## Validation matrix

Built + **fixture-green** (11 unit tests in `google_timeline.rs` +
`spec_validation` 5/5 + workspace `cargo check` + regenerated `schedule_doc` +
bindings clean). The contract-layer trail mapping is **parked**; promotion of
the trail slice to ✅ needs a real export on disk (Needs-sample, no login).

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Import scaffold (format detect + raw preserve + idempotent re-drop) | 🧪 fixture | `detects_each_documented_format`, `on_device_import_preserves_raw_verbatim_and_parks_contract`, `re_dropping_the_same_export_is_idempotent`, `legacy_formats_are_accepted_and_archived`, `imports_location_history_from_a_takeout_zip`, `hub_exposes_an_import_box_and_the_def_is_location_domain`. **David:** Google Maps app → your profile → **Your Timeline → Settings → Export Timeline data** → drop the produced `Timeline.json` (or a legacy Takeout location-history `.json`/`.zip`) into the **Google Timeline** import box → confirm the whole export is archived verbatim under `location/google-timeline/raw/<format>-<hash>.json` and the headline reports the format recognized; re-drop the same file → it is a no-op (no duplicate raw file). **First enable the integration** (it's default-off, privacy-sensitive). |
| Bad-input rejection | 🧪 fixture | `rejects_a_non_location_json`, `rejects_invalid_json`, `rejects_a_zip_without_location_history`. **David:** dropping a non-Timeline JSON / a zip with no location-history file shows a clear error and writes nothing. |
| Contract trail rows (`Fix`) | ⏸ **Needs-sample** | `map_fixes_is_parked_returns_empty_for_every_format` (today: zero rows by design), `write_fixes_day_partitions_and_dedupes_by_guid` (the write seam is correct once fed). **David — to UNPARK:** drop a **real** `Timeline.json` (ideally one **Android** + one **iOS**) and/or a legacy Takeout location-history file under `location/google-timeline/raw/` (an import already puts it there) and flag the agent: the verified nested field names/units let `map_fixes` be filled (the mapping target is documented in its doc-comment). Until then the headline honestly says "trail mapping pending a verified sample". |
| Place-visits → visits contract | ⏸ deferred | Not in scope: semantic place-visits stay raw (await a visits-shaped contract); confirm no `placeVisit` is ever written as a `Fix` row. |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Google Timeline /
Location History (L2276–L2282). Feasibility 🟢 high (for the import path).
Key facts carried forward: Takeout **no longer** carries this data (on-device
since 2024–2025), so the only path is the phone export; iOS export is
reportedly less complete than Android; coordinates are strings not numbers;
Timelinize (Go) is the reference parser supporting both formats — Trove
should accept both so users migrating from old Takeout aren't stranded.
