# Domain: environment

The **write contract** for ambient public feeds — the world around the user.
Air quality (AirNow, WAQI, PurpleAir), pollen (Google Pollen), river and tide
levels (USGS Water, NOAA CO-OPS), buoy waves (NOAA NDBC), station climate
(NOAA CDO), space weather (NOAA SWPC), microphone loudness, earthquakes (USGS),
wildfire detections (NASA FIRMS), weather alerts (NWS), and sun/moon almanacs
(USNO, sunrise-sunset) all land here in three shapes. Each public feed writes
its own `environment/<source>/` folder; readers scan them together. **Owned
home sensors write `home/` with the same reading core (below) and merge at read
time** — public feed vs. owned device is the only thing that decides which
folder. (The pre-taxonomy `weather/` store is grandfathered and stays where it
is — not part of this contract.)

- **Layout:** readings `environment/<source>/YYYY-MM.jsonl` (month of `ts`) ·
  geo-events `environment/<source>/events/YYYY-MM.jsonl` (month of `ts`) ·
  almanacs `environment/<source>/almanac/YYYY-MM.jsonl` (month of `date`)
- **Kind:** append-only event streams (all three)
- **Schemas:**
  [`schemas/environment.reading.schema.json`](../schemas/environment.reading.schema.json),
  [`schemas/environment.geo-event.schema.json`](../schemas/environment.geo-event.schema.json),
  [`schemas/environment.almanac.schema.json`](../schemas/environment.almanac.schema.json)
- **Dedupe key:** reading & geo-event by `guid` (source-unique — typically
  `station/site + metric + ts`, the USGS event id, the alert id); almanac by
  `date` + rounded `lat`/`lon`. Imports must skip already-stored keys.

A source picks the shape its data fits — most write only one. Source folders
are discovered by scanning: creating `environment/<source>/` is the
registration, no code change.

## The reading — `<source>/YYYY-MM.jsonl`

A scalar measurement: one `metric`, one numeric `value`, at a place and time.
This is the **shared scalar-reading core** — `home/`-owned-sensor readings use
the identical four required fields, so a public AQI feed and an owned indoor
sensor land the same shape and merge at read time. Only `ts`/`source`/`metric`/
`value` are required; a sparse feed (a bare dB sample) writes just those four.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time of the observation |
| `source` | string | ✔ | collector id, = the folder name |
| `metric` | string | ✔ | what was measured, snake_case: `temperature`, `humidity`, `pm25`, `pm10`, `ozone`, `no2`, `so2`, `co`, `aqi`, `pollen`, `kp`, `water_level`, `discharge`, `wave_height`, `noise_db`, `pressure`, `uv`, … |
| `value` | number | ✔ | the measured value |
| `unit` | string | | unit of `value`: `C`, `F`, `percent`, `ppm`, `ug_m3`, `aqi`, `db`, `ft`, `cfs`, `m`, `hpa`, `index`, … |
| `place` | string | | human label of where (station / city / reporting-area name) |
| `lat` | number | | latitude of the observation |
| `lon` | number | | longitude of the observation |
| `station` | string | | source-native station / site / sensor id |
| `guid` | string | | source-unique id, the dedupe key |
| `extra` | object | | everything source-specific (AQI category, raw concentration, flood-stage, per-pollutant subindices, …) |

## The geo-event — `<source>/events/YYYY-MM.jsonl`

A located, dated hazard or phenomenon — distinct from a reading because it is a
discrete event with a magnitude/severity rather than a continuous metric.
Earthquakes, wildfire detections, and weather/geomagnetic alerts converge here.
`event_type` is an **open vocabulary** (so `tsunami`, `volcano`, … can arrive
additively without a new folder); common values are `quake`, `fire`, `alert`.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the event occurred (quake origin time, detection acquisition, alert onset) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id (USGS event id, alert id, detection hash) — the dedupe key |
| `event_type` | string | ✔ | open vocabulary: `quake` \| `fire` \| `alert` \| … |
| `magnitude` | number | | quake magnitude (or any scalar intensity the event carries) |
| `place` | string | | human place description (`"12km NE of Ojai, CA"`, the alert area) |
| `lat` | number | | latitude |
| `lon` | number | | longitude |
| `severity` | string | | source-native severity (`"Severe"`, `"Extreme"`, alert level) |
| `headline` | string | | one-line human summary (alert headline) |
| `url` | string | | canonical link to the event page |
| `expires` | string | | RFC3339 local; when an alert stops being active |
| `extra` | object | | everything source-specific (depth_km, tsunami flag, felt reports, fire confidence/FRP/satellite, …) |

## The almanac — `<source>/almanac/YYYY-MM.jsonl`

One day of solar and lunar geometry for a location: sun/twilight times, day
length, moon. Keyed by `date` (+ rounded coords) so a re-pull is idempotent.
Only `date`/`source` are required — a sun-only feed omits every moon field.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `date` | string | ✔ | the local calendar day (`YYYY-MM-DD`) |
| `source` | string | ✔ | collector id, = the folder name |
| `lat` | number | | latitude the geometry was computed for |
| `lon` | number | | longitude |
| `sunrise`, `sunset` | string | | RFC3339 local |
| `solar_noon` | string | | RFC3339 local |
| `civil_twilight_begin`, `civil_twilight_end` | string | | RFC3339 local |
| `nautical_twilight_begin`, `nautical_twilight_end` | string | | RFC3339 local |
| `astronomical_twilight_begin`, `astronomical_twilight_end` | string | | RFC3339 local |
| `day_length` | string | | duration, source-native (`"14:21"`) |
| `golden_hour` | string | | RFC3339 local; start of evening golden hour where the source reports it |
| `moonrise`, `moonset` | string | | RFC3339 local |
| `moon_phase` | string | | named phase (`"Waning Gibbous"`) |
| `extra` | object | | everything source-specific (illumination fraction, named moon-phase times, …) |

Omit empty fields throughout. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T13:00:00-07:00","source":"airnow","metric":"pm25","value":42,"unit":"aqi","place":"Los Angeles-North Main Street","lat":34.0667,"lon":-118.2269,"station":"060371103","guid":"airnow:060371103:pm25:2026-06-10T13:00","extra":{"category":"Good","raw_concentration":10.1,"raw_unit":"ug_m3"}}
{"ts":"2026-06-10T14:35:00-07:00","source":"usgs-water","metric":"discharge","value":1240,"unit":"cfs","station":"11447650","lat":38.4564,"lon":-121.5008,"guid":"usgs-water:11447650:00060:2026-06-10T14:35","extra":{"flood_stage":"action","parameter_code":"00060"}}
{"ts":"2026-06-10T14:05:00-07:00","source":"macos-microphone","metric":"noise_db","value":48.2,"unit":"db"}
```

```jsonl-event
{"ts":"2026-06-10T03:42:11-07:00","source":"usgs-earthquakes","guid":"ci40123456","event_type":"quake","magnitude":4.2,"place":"12km NE of Ojai, CA","lat":34.5483,"lon":-119.1817,"url":"https://earthquake.usgs.gov/earthquakes/eventpage/ci40123456","extra":{"depth_km":8.3,"alert":"green","tsunami":false,"felt":214}}
{"ts":"2026-06-10T11:00:00-07:00","source":"nws","guid":"urn:oid:2.49.0.1.840.0.abc123","event_type":"alert","severity":"Severe","headline":"Red Flag Warning issued June 10 at 11:00AM PDT until June 11 at 8:00PM PDT","url":"https://api.weather.gov/alerts/urn:oid:2.49.0.1.840.0.abc123","expires":"2026-06-11T20:00:00-07:00","place":"Ventura County Mountains"}
{"ts":"2026-06-10T13:24:00-07:00","source":"nasa-firms","guid":"firms:VIIRS_SNPP_NRT:34.421:-118.903:2026-06-10:1324","event_type":"fire","lat":34.421,"lon":-118.903,"extra":{"confidence":"high","frp":18.6,"satellite":"N","daynight":"D"}}
```

```jsonl-almanac
{"date":"2026-06-10","source":"usno","lat":34.05,"lon":-118.25,"sunrise":"2026-06-10T05:42:00-07:00","sunset":"2026-06-10T20:03:00-07:00","solar_noon":"2026-06-10T12:52:00-07:00","civil_twilight_begin":"2026-06-10T05:14:00-07:00","civil_twilight_end":"2026-06-10T20:31:00-07:00","day_length":"14:21","moonrise":"2026-06-10T01:18:00-07:00","moonset":"2026-06-10T13:47:00-07:00","moon_phase":"Waning Gibbous","extra":{"illumination":0.78}}
{"date":"2026-06-10","source":"sunrise-sunset","lat":40.71,"lon":-74.01,"sunrise":"2026-06-10T05:24:00-04:00","sunset":"2026-06-10T20:26:00-04:00","solar_noon":"2026-06-10T12:55:00-04:00","day_length":"15:02","golden_hour":"2026-06-10T19:39:00-04:00"}
```

## Read-time semantics (FYI for writers)

Readings are the join point with `home/`: the environment reader and the home
reader both surface `{ts, source, metric, value, …}` rows, and a unified view
groups by `metric` regardless of which folder wrote them — so keep `metric`
names and `unit`s consistent with the `home/` contract (they were co-designed
to share this core). Write the value the source gave you with its real `unit`;
don't normalize C↔F or AQI scales at write time — that's a read-time opinion.
Geo-events sit on the timeline as discrete points (a quake, a fire, an alert);
an alert's `expires` lets a reader show what was active at a given moment
without inventing an end it can't observe. Almanacs are reference geometry, one
row per day per location — a reader joins "when was sundown the day of this
photo" by date. Each public feed writes only its own folder and its own stable
keys; cross-source reconciliation (which AQI station wins, dedup across
overlapping feeds) is read-time work, never baked in at write time.
