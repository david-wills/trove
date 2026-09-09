# Domain: travel

Trips broken into their segments — flights, lodging, car rentals, trains —
in one normalized store, whatever assembled them. Flighty and
myFlightRadar24 (flight logs), TripIt (the richest: every segment type),
Airbnb (lodging stays), and the derived `flight-emails` extractor (flights
parsed from confirmation mail already in the vault) all write this shape; a
reader stitches segments back into trips and onto the timeline at read time.
Each source writes its own folder with its own stable `guid`s — two sources
that captured the same flight both write it, and reconciliation is a
read-time opinion, never a write-time merge.

- **Layout:** `travel/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/travel.segment.schema.json`](../schemas/travel.segment.schema.json)
- **Dedupe key:** `guid` (source-unique: ICS UID, Airbnb confirmation code,
  Flighty row id, a `flight-emails` booking-ref+segment, a stable
  date+route+number hash where the source carries no id). Imports must skip
  already-stored guids.

## Segment

One trip segment per line — the `type` discriminator says which kind, and
the same handful of shared fields describe all of them. Only
`ts`/`source`/`type`/`guid` are required; a sparse source (a CSV flight log
with date + airports and nothing else) writes a minimal line, while a rich
source (TripIt, with terminals and seats) fills more. Type-specific detail
with no shared column — seat, terminal, room, address, car class, fare,
delay minutes, aircraft — goes under `extra`; full fidelity always survives
in the source's own `travel/<source>/raw/` folder regardless.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local; the segment's start (departure, check-in, pickup) — the partition key |
| `source` | string | ✔ | collector id, = the folder name |
| `type` | string | ✔ | `"flight"` \| `"lodging"` \| `"car"` \| `"train"` \| `"ferry"` \| `"bus"` \| `"cruise"` \| `"transfer"` \| `"activity"` \| `"other"` |
| `guid` | string | ✔ | source-unique id, the dedupe key |
| `end_ts` | string | | RFC3339 local; arrival / check-out / drop-off, when known |
| `start_place` | string | | origin: airport/station code (IATA where it has one) or city — the departure/check-in place |
| `start_place_name` | string | | display name for `start_place` (airport name, hotel/listing name, city label) |
| `end_place` | string | | destination: airport/station code or city (flights, trains, transfers, one-way car rentals) |
| `end_place_name` | string | | display name for `end_place` |
| `vendor` | string | | operating brand: airline, hotel/lodging brand, rental company, rail operator |
| `number` | string | | flight or train number (the one shared typed field; verbatim, e.g. `"UA 523"`) |
| `confirmation` | string | | confirmation / record-locator / booking code shown to the traveller |
| `booking_id` | string | | groups segments of one itinerary/trip (a multi-leg booking, a `flight-emails` booking ref) |
| `status` | string | | source-native state — e.g. `"confirmed"`, `"canceled"`, `"completed"`, `"delayed"` (open string; sources differ) |
| `extra` | object | | everything source-specific (seat, terminal, gate, room, address, car class, amount, delay minutes, aircraft, weather, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-07-14T08:25:00-07:00","source":"tripit","type":"flight","guid":"item-1847562301-1","end_ts":"2026-07-14T16:58:00-04:00","start_place":"SFO","start_place_name":"San Francisco Intl","end_place":"JFK","end_place_name":"John F. Kennedy Intl","vendor":"United Airlines","number":"UA 523","confirmation":"H4X9Q2","booking_id":"trip-298104","status":"confirmed","extra":{"seat":"14C","cabin":"Economy","departure_terminal":"3","aircraft":"Boeing 737-900"}}
{"ts":"2025-11-02T09:40:00+01:00","source":"myflightradar24","type":"flight","guid":"mfr24-3f9a1c7e","start_place":"LHR","end_place":"AMS"}
{"ts":"2026-08-03T15:00:00+02:00","source":"airbnb","type":"lodging","guid":"HMABCDEFGH","end_ts":"2026-08-09T11:00:00+02:00","start_place":"Lisbon, PT","start_place_name":"Sunny Alfama Loft with River View","confirmation":"HMABCDEFGH","extra":{"amount":"742.00","currency":"EUR","nights":"6","country":"Portugal"}}
```

## Read-time semantics (FYI for writers)

The travel reader scans `travel/*/` — creating your source folder is the
registration — groups segments into trips (by `booking_id`, then by
date/place adjacency) and merges segments two sources both captured (the
same flight from Flighty and `flight-emails`) by `guid` and route, keeping
`source` provenance in the output. A segment is one record: never split a
flight's gate or a stay's address into another folder; the location view
joins a segment's places at read time rather than the writer emitting a
separate location row. Write `ts` as the local segment start so partitions
and timelines line up; write the codes you observed (IATA, confirmation)
raw and leave resolution to the reader.
