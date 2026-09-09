# TripIt

- **id:** `tripit`
- **domains:** `travel/` (contract: **Phase 3 pending** — trip-segment shape:
  flights, hotels, cars, trains)
- **status:** 🧪 built (parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (user downloads ICS/PDF via TripIt Settings → Download
  Your Data; no live connection)
- **connection:** none (file import; the public API is closed to new app
  registrations — see research notes)
- **evidence:** official — TripIt GitHub issue #288 (May 2024) confirms the
  API is closed to new integrations; ICS export documented via Settings →
  Download Your Data. ICS shape parses with a generic ICS parser.
- **effort / priority:** M / P2
- **needs:** Needs-sample (TripIt's exported ICS field layout — confirm
  segment-type encoding before finalizing the parser)

## What it is

Trip-itinerary aggregator: users forward booking confirmation emails (or
auto-import them) and TripIt assembles trips with flight, hotel, car-rental,
and train segments, each with dates, times, and confirmation numbers. The
data matters because it's a clean, pre-parsed travel timeline — exactly the
trip-segment shape the `travel/` domain wants.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Trips | all plans | name, start/end, destination | official API docs (model) |
| Flight segments | all plans | airline, flight no, dep/arr airport + times, confirmation | official API docs |
| Hotel / car / train segments | all plans | name, check-in/out or pickup, confirmation | official API docs |

Delivered via the ICS export, so each segment arrives as a VEVENT; richer
JSON-LD fields are not in the ICS — what the ICS omits is simply absent
(omit-if-empty in the contract).

## Access & auth

- **Import only.** User goes to TripIt Settings → Download Your Data, which
  produces an ICS (calendar) export (and PDFs). No machine-readable JSON
  export for end users.
- The TripIt public REST API (tripit.github.io/api/doc/v1) still exists and
  returns the full trip/segment model, but **OAuth app registration is closed
  to new developers** (issue #288, May 2024) — existing keys only. Not a path
  for a shipping standalone collector.
- No TCC; user-selected file. Standalone-clean (parse on import, no network).

## Vault mapping

- **Raw layer:** `travel/tripit/raw/<imported-file>.ics` (and any PDFs kept
  as sidecar artifacts) — the original export, full fidelity.
- **Contract layer:** `travel/tripit/YYYY-MM.jsonl` per the (pending)
  trip-segment contract — expected shape: one row per segment (`ts` = segment
  start, `source`, `guid` = ICS UID, `segment_type` flight/hotel/car/train,
  `start`/`end`, `origin`/`destination` or location, `confirmation`),
  overflow in `extra`. Contract not yet ratified — normalized layer is
  **parked behind Needs-David (contract)**; raw ICS can land first.
- **Dedupe:** ICS UID as `guid`; import is idempotent by UID + source file.

## Build plan

1. Module `crates/trove-core/src/tripit.rs`: `DEF` (Import — file box, per the
   `letterboxd.rs` reference). No connection.
2. Registration line in `INTEGRATIONS`.
3. **Parser-last / Needs-sample:** the ICS export field layout (how TripIt
   encodes segment type, confirmation numbers, multi-segment trips into VEVENT
   SUMMARY/DESCRIPTION/CATEGORIES) is not formally documented — obtain a real
   sample export before finalizing the parser; lean on a generic ICS parser
   for the envelope.
4. Store + import idempotency tests on the sample, unique temp dirs.
5. Normalized writes via `store` once the trip-segment contract is ratified.

## Build notes (2026-06-21)

Module `crates/trove-core/src/tripit.rs` replaces the `NotWired` stub with a
full `Behavior::Import` scaffold. Contract binding is in place (`Segment`,
`travel/tripit/YYYY-MM.jsonl`, raw layer at `travel/tripit/raw/`). The
`TripEvent → Segment` mapping (`event_to_segment`) is complete and tested
(11 tests, all green). The ICS → `TripEvent` parser (`events_from_ics`) is
parked pending a real `.ics` sample — the ICS VEVENT field layout (how TripIt
encodes segment type, confirmation numbers, and X-TRIPIT-* extension fields) is
not formally documented. The scaffold surfaces a clear `PARKED_MSG` rather than
guessing the field shape (evidence rule). No new crate deps were added (`ical`
already present for caldav.rs). No new connection — import-only, no login.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Trip / segment import | 🧪 parked | drop a real TripIt `.ics` export into the import box; fill `events_from_ics`; confirm segments in `travel/tripit/` + hub last-data |
| Segment-type fidelity | 🧪 parked | confirm flight vs. hotel vs. car rows correctly typed against a real sample |
| Contract mapping | ✅ tested | `event_to_segment` unit tests green (11/11) |
| Dedupe / re-import | ✅ tested | idempotency test verifies UID-keyed dedup |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §TripIt (L2388–L2394).
Feasibility 🟠 low for the API (closed to new apps), 🟢 viable as ICS import.
The flight-confirmation-email parser (`flight-emails`) and Flighty cover
overlapping ground; TripIt's value is for users who already centralize trips
there. TripIt Pro real-time alerts are irrelevant for data capture. The
generic ICS path the calendar work already needs can be reused here.
