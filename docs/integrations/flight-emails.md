# Flight Confirmation Emails (derived)

- **id:** `flight-emails`
- **domains:** `travel/` (contract: **Phase 3 pending** — trip-segment shape:
  flights, hotels, cars, trains)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import — a derived extractor pass over email already in the
  vault (not a new collector; idempotent by booking reference).
- **connection:** none — it post-processes email the shipped email import
  already wrote; no new login or network.
- **evidence:** community — airline confirmation emails carry schema.org
  `FlightReservation` JSON-LD and/or ICS attachments with structured
  departure/arrival data
- **effort / priority:** M / P2
- **needs:** **privacy** (parses message bodies — ships opt-in with explicit
  acknowledgement) · travel contract not yet ratified (Needs-David)

## What it is

A **derived** source: rather than connecting to an airline, this is an
extractor pass that reads confirmation emails the user already imported and
turns them into structured flight records. It covers two populations at once —
people who use Flighty/TripIt as a downstream aggregator, and people who use no
flight app at all but whose itineraries sit in their inbox. Because it reuses
the email already in the vault, it adds no new collection surface, only a
parsing step.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Flight segments | n/a | airline, flight number, origin/dest IATA, dep/arr times, booking ref | community (JSON-LD / ICS) |
| JSON-LD path | n/a | schema.org `FlightReservation` (no regex) | community |
| ICS-attachment path | n/a | VEVENT dep/arr from airline-attached `.ics` | community |

All optional in the contract (omit-if-empty). Coverage varies by airline; a
record carries only the fields its source email actually contained.

## Access & auth

- Input: the already-imported email (`.mbox`/correspondence) in the vault — no
  airline API, no mailbox re-fetch.
- Extraction: pattern-match sender domains (aa.com, united.com, delta.com,
  southwest.com, alaskaair.com, britishairways.com, lufthansa.com, emirates.com,
  aircanada.com, …) plus a generic ICS/JSON-LD path that doesn't depend on the
  sender. Prefer schema.org `FlightReservation` JSON-LD (machine-readable, no
  regex); fall back to ICS VEVENT attachments; airline-specific HTML regex
  last.
- Auth: none. Standalone-clean (a local parse).

## Vault mapping

- **Raw layer:** `travel/flight-emails/raw/` — the extracted candidates with
  provenance (source message guid, which extraction path fired), partitioned by
  month of departure.
- **Contract layer:** `travel/flight-emails/YYYY-MM.jsonl` per the (pending)
  travel trip-segment contract — expected shape: one row per flight segment
  (`ts` = departure, `source`, `guid` = booking-ref + segment, `airline`,
  `flight_no`, `origin`, `dest`, `dep`, `arr`), overflow in `extra`. Parked
  behind **Needs-David (contract)** until the travel shape is ratified.
- **Dedupe:** booking reference + segment as `guid` so the same itinerary
  parsed from multiple emails (booking + reminder + check-in) collapses to one
  segment. Does **not** consume an email row — the correspondence row stays
  whole; this derives a sibling travel record.

## Build plan

1. **Sequence after the email import is stable** — this pass has no value
   until there's email in the vault to read.
2. Module `crates/trove-core/src/flight_emails.rs`: `DEF` (Import / derived
   pass), triggered by Sync-now and after email imports.
3. Extraction priority: JSON-LD `FlightReservation` → ICS VEVENT → per-airline
   HTML regex. Cover the major carriers listed above + generic ICS/JSON-LD.
4. **Privacy gate:** ships **opt-in** — it parses message bodies; require
   explicit acknowledgement on enable. NotWired/disabled by default until
   acknowledged.
5. Fixtures: sample confirmation emails per extraction path (JSON-LD, ICS,
   regex-only); extractor + store + booking-ref-dedupe tests, unique temp dirs.
6. Vault writes via `store` helpers once the travel contract is ratified.
7. AwardWallet's commercial Email Parsing API is the managed fallback if the
   in-house extraction proves too brittle — but it is a separate (`awardwallet`)
   provider, not this one.

## Build notes (2026-06-17)

**Shipped as `Behavior::Import` accepting `.mbox` files** — not a vault-pass
over stored messages. The vault's email store keeps only the plain-text body and
attachment metadata; JSON-LD is in the HTML body, ICS bytes are in attachment
content — neither is stored in the vault's `Message` records. The practical path
is a fresh parse of the original mbox (same file as the email import), which
makes the extraction full-fidelity and independent of the correspondence importer.

**Two extraction paths implemented and unit-tested:**
1. JSON-LD `FlightReservation` from the HTML body (`body_html(0)`) — field names
   verified against schema.org/FlightReservation and schema.org/Flight.
2. ICS VEVENT from calendar attachments (`attachment(i).text_contents()`) — standard
   RFC 5545 fields plus common airline X- extensions (X-CONFNUM, X-FLIGHTNO, etc.).

JSON-LD wins when both are present; ICS is the fallback. HTML regex is intentionally
not implemented (too brittle; skip > guess).

**Dedupe guid:** `{reservationId}-{originIATA}-{destIATA}` — stable across
booking + reminder + check-in emails for the same itinerary. A candidate with no
departure time is silently skipped (cannot partition).

**Parser parked: NO** — both extraction paths are implemented and tested.
**Needs-sample: NO** — JSON-LD and ICS are documented, standardized formats.
JSON-LD field names verified from schema.org. ICS VEVENT verified from RFC 5545.

**Narrows from brief:** HTML regex per-airline deliberately omitted. The vault-pass
variant (reading stored Messages) deferred until the email store saves HTML + ICS bytes.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| JSON-LD extraction | ✅ built + tested | import a real airline confirmation `.mbox` with schema.org markup; confirm a segment row in `travel/flight-emails/` |
| ICS-attachment extraction | ✅ built + tested | import a `.mbox` whose calendar attachment (`.ics`) carries the flight VEVENT; confirm dep/arr parsed |
| Multi-email dedupe | ✅ built + tested | import a `.mbox` with booking + reminder + check-in for one itinerary; confirm one segment, not three |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Flight Confirmation Email
Parsing (L2396–L2402). Feasibility 🟢 high — major carriers follow predictable
patterns and increasingly embed schema.org `FlightReservation` JSON-LD (no
regex) plus ICS attachments. Build **later**, once the email `.mbox` import is
stable. Google/Apple Mail already auto-extract the same structured data into
Trips/Calendar, confirming it's reliably machine-readable. AwardWallet's free
Email Parsing API tier is a managed alternative if extraction gets too complex.
Privacy-sensitive (message bodies): opt-in with explicit acknowledgement.
