# Airbnb

- **id:** `airbnb`
- **domains:** `travel/` (contract **ratified** as of this build — trip-segment
  shape: flights, hotels, cars, trains; lodging stays are the lodging segment.
  `airbnb` is the travel domain's pioneer/first collector. Rust type `Segment`,
  registered in `DOMAINS`, schema `travel.segment.schema.json`.)
- **status:** 🧪 built (fixture-tested, not validated) — contract bound + import
  scaffold shipped; **parser parked pending a real export sample**.
- **unavailable_reason:** none
- **behavior:** Import (user-supplied GDPR data ZIP or web booking CSV; no
  guest API for ongoing pull)
- **connection:** none — the user runs Airbnb's web export themselves; Trove
  never logs in.
- **evidence:** community-schema — guest CSV poorly documented and
  region-varying; GDPR ZIP (`reservations.json`) is the reliable path
  (medium confidence) · sample-required for the exact `reservations.json`
  shape
- **effort / priority:** S / P2
- **needs:** privacy (booking history is a location/travel trail — where you
  stayed and when — opt-in with explicit acknowledgement; default-off) ·
  **Needs-sample** — the GDPR ZIP `reservations.json` layout is not officially
  documented, so `airbnb::stays_from_export` is parked behind a clear
  Needs-sample error rather than parsing a guessed shape (evidence rule).
  The travel contract this writes into is already ratified; only the
  export-file field mapping awaits a sample.

## What it is

Airbnb is a lodging marketplace; guest booking history records where a user
stayed, the dates, the city/country, the confirmation code, and the amount
paid. For Trove it's a travel-history source — the lodging counterpart to
flight data — letting the timeline answer "where was I sleeping that week."
Anyone who travels and books stays has this; it's a meaningful chunk of a
personal location/travel record that is otherwise uncaptured.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Past reservations | all accounts | confirmation code, check-in, check-out, listing name, city, country, amount paid | community (web CSV) |
| Full booking history | all accounts | same fields, all years (GDPR ZIP is more complete than the web CSV) | community (`reservations.json`) |

All optional in the contract (omit-if-empty); a stay with no price recorded
simply carries no amount.

## Access & auth

- **No public guest API.** Two manual export mechanisms:
  - Web CSV: Login → Trips → Past → "See all reservations" → CSV export.
    Availability varies by account region; the ICS calendar export was
    removed in a May 2025 update (that change targeted hosts).
  - GDPR data download: Privacy Settings → "Request Your Personal Data"
    → ZIP containing `reservations.json` with all booking history. This is
    the preferred, more complete path.
- Auth: none in Trove — the user downloads the file and hands it to the
  importer. Standalone-clean (no login, no scraping, no runtime dependency).
- No TCC; the user picks a local file in the import box.

## Vault mapping

- **Raw layer:** `travel/airbnb/raw/YYYY.jsonl` — the export object for each
  stay, preserved verbatim at full fidelity, partitioned by check-in year
  (`write_raw`). Nothing the export carried is dropped.
- **Contract layer:** `travel/airbnb/YYYY-MM.jsonl` (partitioned by the local
  month of check-in) per the ratified travel trip-segment contract — one
  `Segment` per stay as a lodging segment:
  - `ts` = check-in at listing-local **noon** (the export carries a date, not a
    time; noon avoids midnight-boundary surprises — the `letterboxd` precedent),
  - `source` = `"airbnb"`, `type` = `"lodging"`, `guid` = `confirmation` = the
    Airbnb confirmation code,
  - `end_ts` = check-out at listing-local 11:00, `start_place` = city,
    `start_place_name` = listing name, `confirmation` = the code, `status` =
    source-native state where present,
  - `extra` = `amount` / `currency` / `nights` / `country` / `host` (money stays
    a verbatim string under `extra`, the travel-contract idiom).
  Records route whole — a stay is one travel record even though it also implies
  a location.
- **Dedupe:** confirmation code as `guid`; the import reads existing segment
  guids first and skips any already stored, so re-importing a newer export is
  idempotent.

## Build plan

1. **Done** — travel contract pioneered: `Segment` Rust type
   (`crates/trove-core/src/travel.rs`), `travel` registered in `DOMAINS`,
   `travel.segment` promoted from Phase-3 draft to ratified (round-trip + doc
   sync in `spec_validation.rs`).
2. **Done** — module `crates/trove-core/src/airbnb.rs`: `DEF` upgraded
   `NotWired → Import` (import-box copy points at the GDPR ZIP, with the web-CSV
   fallback), the `Stay → Segment` contract mapping (`stay_to_segment`), the raw
   layer (`write_raw`), guid dedupe, and the re-runnable import loop — all
   tested. No connection (no auth). Registration line in `INTEGRATIONS` already
   present.
3. **Parked — Needs-sample:** `airbnb::stays_from_export` (the only piece that
   reads the export file) returns a clear Needs-sample error. The
   `reservations.json` layout is undocumented and region-varying; per the
   evidence rule (cf. the raindrop `_id` bug) Trove will not parse a guessed
   shape. When a real `reservations.json` (or Trips CSV) sample lands, populate
   `Stay` (and its `raw` map) field-for-field there — nothing downstream changes.
4. **Done** — privacy gate: ships opt-in (`default_on: false`), travel/location
   trail.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Travel contract bind (`Segment` / `travel` domain) | 🧪 fixture-tested | `cargo test -p trove-core` — `fixtures_validate_against_schemas_and_rust_types` round-trips `Segment` against `travel.segment.schema.json`; the airbnb unit tests prove `stay_to_segment`, the raw layer, dedupe, omit-if-empty, and the parked-import error. |
| `Stay → Segment` mapping (lodging) | 🧪 fixture-tested | covered by `airbnb::tests::stay_maps_to_a_lodging_segment_on_the_travel_contract` (controlled values, not a fabricated wire shape). |
| Booking history (GDPR ZIP) | ⛔ **parked — Needs-sample** | **(Needs-David sample)** Request the Airbnb data download (Account → Privacy & sharing → Request your personal data), then drop the real `reservations.json` (the file inside the ZIP) at a path the agent can read so the field mapping in `airbnb::stays_from_export` can be pinned to the actual wire names. Until then, importing the ZIP surfaces the Needs-sample message (by design — Trove will not parse a guessed shape). |
| Web CSV fallback | ⛔ **parked — Needs-sample** | **(Needs-David sample, region-permitting)** Export the Trips → Past CSV where your region offers it and drop it alongside the JSON sample; the same `stays_from_export` seam will handle the CSV columns. |

**End-to-end validation (once a sample lands and the parser is unparked):**
enable Airbnb in the hub (it ships opt-in — acknowledge the travel/location-trail
prompt), import the ZIP/JSON in the import box, and confirm one row per stay in
`travel/airbnb/YYYY-MM.jsonl` (`type:"lodging"`, `guid` = the confirmation code,
`ts` = check-in noon), the verbatim export under `travel/airbnb/raw/YYYY.jsonl`,
and the hub card's last-data. Re-import the same file and confirm 0 new rows
(idempotent dedupe by `guid`).

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Airbnb (Guest Booking
History) (L2428–L2434). Feasibility 🟡 medium. No guest API — both paths are
manual web exports; the GDPR ZIP is the reliable one, the web CSV varies by
region and the UI is unstable for scraping (we don't scrape — accept a
user-provided file). Low priority versus flight and GPS sources. Flighty /
TripIt / flight-email parses share the travel contract — sequence one
flight source alongside Airbnb to exercise both segment kinds.
