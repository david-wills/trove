# Domain: location

Standalone GPS trails — every location fix, from any logger, in one stream.
Continuous loggers (OwnTracks, Overland), movement segments (Google Timeline,
Arc trips), GPX/FIT imports, and sparse vehicle pings (Smartcar, Tesla) all
write one record per fix; readers reconstruct a path by grouping the fixes
that share a `trail` id, and sort by `ts`. This is **privacy-sensitive** data
(a continuous trail of where the owner has been): every source ships opt-in
with explicit acknowledgement. Workout-embedded GPS routes are **not** here —
a Strava/Garmin/Apple-Health workout is one whole `health/` record whose route
the location view joins at read time.

- **Layout:** `location/<source>/YYYY-MM-DD.jsonl` (day of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/location.fix.schema.json`](../schemas/location.fix.schema.json)
- **Dedupe key:** `guid` (source-unique: a fix id, a trip-id+seq, or a
  `(ts,lat,lon)` hash where the source gives no id). Overlapping logger
  batches must skip already-stored guids before appending.

## Fix

One GPS fix per line. A track / logger batch / movement segment / vehicle
trip is **the set of fixes sharing one `trail` id** — never a points array,
so the high-volume stream stays day-partitionable and a path is a read-time
grouping. Only `ts`, `source`, `lat`, `lon` are required; a sparse vehicle
ping writes just those (plus its odometer in `extra`), a rich logger fills
the rest.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time of the fix |
| `source` | string | ✔ | collector id, = the folder name |
| `lat` | number | ✔ | latitude, decimal degrees (WGS84) |
| `lon` | number | ✔ | longitude, decimal degrees (WGS84) |
| `ele` | number | | elevation in metres |
| `speed` | number | | ground speed in metres/second |
| `accuracy` | number | | horizontal accuracy radius in metres |
| `heading` | number | | course over ground in degrees (0–360, clockwise from north) |
| `trail` | string | | stable id grouping the fixes of one track / batch / segment / trip; the read-time grouping key |
| `trail_name` | string | | display name for the trail (GPX track name, segment label) |
| `mode` | string | | movement mode where the source classifies it: `"walking"`, `"cycling"`, `"driving"`, `"transit"`, … (verbatim from the source) |
| `guid` | string | | source-unique id, the dedupe key |
| `extra` | object | | everything source-specific (odometer, charge, battery, motion type, wifi SSID, place confidence, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T08:14:03-07:00","source":"owntracks","lat":37.77493,"lon":-122.41942,"ele":28.4,"speed":1.3,"accuracy":5,"heading":92,"trail":"phone-2026-06-10","guid":"ot-1718031243-37.77493--122.41942","extra":{"batt":74,"topic":"owntracks/dave/iphone"}}
{"ts":"2026-06-10T08:31:50-07:00","source":"google-timeline","lat":37.80331,"lon":-122.44896,"trail":"seg-2026-06-10T08:22:00","mode":"cycling","guid":"gt-2026-06-10T08:22:00-cycling","extra":{"distance_m":4120,"point_confidence":"high"}}
{"ts":"2026-06-10T18:02:11-07:00","source":"smartcar","lat":37.4419,"lon":-122.143,"extra":{"odometer_km":48213.6,"make":"TESLA"}}
```

## Read-time semantics (FYI for writers)

The location reader scans `location/*/`; creating your source folder is the
registration. A path is reconstructed by grouping fixes that share
`source`+`trail` and sorting by `ts` — sources with no native segmentation
(raw loggers) may omit `trail` entirely and let the reader segment by time
gaps. Vehicle pings are sparse points, not a trace; the reader interpolates
nothing it can't observe. **Visits and saved places live elsewhere:** Swarm
check-ins and Google Maps saved places are place/visit-shaped, not fixes —
they stay per-source raw under `location/<source>/` until a visits-shaped
contract lands, and are never forced into this fix row. Write handles and
coordinates as observed; never persist a derived path back into the vault.
