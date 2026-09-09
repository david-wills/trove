# Domain: home

Everything an **owned** smart device or sensor records about the home: air
and climate readings, discrete device events (a lock turning, a doorbell
ringing, a vacuum running, a voice command), and metered energy and water.
Indoor-air monitors (Airthings, Awair, Aranet, Netatmo, SwitchBot),
personal weather stations (Tempest, Ambient Weather), thermostats (Nest,
Honeywell), lighting (Hue, Lutron), locks (August/Yale), cameras (Ring),
energy monitors (Sense, Emporia, Enphase, Tesla, Kasa, Green Button), the
Home Assistant superconnector, and the Alexa voice export all write these
shapes; readers see one home stream across every vendor. The split from
`environment/` is ownership, not shape — a sensor you own writes `home/`, a
public feed writes `environment/`, and the two **merge at read time**
because their reading core is identical (see below).

- **Layout:** `home/<source>/YYYY-MM.jsonl` (readings, month of `ts`) +
  `home/<source>/events/YYYY-MM.jsonl` (device events) +
  `home/<source>/energy/YYYY-MM.jsonl` (energy/water intervals). A source
  writes only the substreams it produces; a per-source `raw/` folder for
  full native fidelity is always allowed alongside.
- **Kind:** append-only event streams (devices keep little or no history;
  Trove accumulates it by polling)
- **Schemas:**
  [`schemas/home.reading.schema.json`](../schemas/home.reading.schema.json),
  [`schemas/home.event.schema.json`](../schemas/home.event.schema.json),
  [`schemas/home.energy.schema.json`](../schemas/home.energy.schema.json)
- **Dedupe key:** `guid` (source-unique) where the source supplies an event
  id; otherwise the natural key (`device`/`circuit` + `metric` + `ts`,
  synthesized identically on every re-run) keeps polled and back-filled rows
  from duplicating.

## The reading — `home/<source>/YYYY-MM.jsonl`

A single scalar observation: temperature, humidity, CO2, PM2.5, radon, VOC,
noise, pressure, a thermostat's ambient temp, a Hue Motion sensor's degrees.
The **four-field core (`ts`, `source`, `metric`, `value`) is shared verbatim
with the `environment/` domain** so owned-device and public-feed readings of
the same metric stack in one read-time view. `home/` rows add the optional
`device`; `environment/` rows more often carry `lat`/`lon` and a `station`.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the reading was taken |
| `source` | string | ✔ | collector id, = the folder name |
| `metric` | string | ✔ | what was measured, snake_case: `temperature`, `humidity`, `co2`, `pm25`, `pm10`, `radon`, `voc`, `noise_db`, `pressure`, `aqi`, `uv`, `water_level`, … |
| `value` | number | ✔ | the numeric reading |
| `unit` | string | | `C`, `F`, `percent`, `ppm`, `ug_m3`, `bq_m3`, `db`, `hpa`, `aqi`, `index`, … — omit only when truly unitless |
| `place` | string | | human label of where: room name (`"Bedroom"`), station name |
| `device` | string | | the owned device/sensor id or name (home-specific) |
| `lat`, `lon` | number | | location when the device reports it (outdoor PWS) |
| `extra` | object | | everything source-specific (battery, score, raw index, …) |

## The device event — `home/<source>/events/YYYY-MM.jsonl`

A discrete thing that happened: a lock unlocked, a doorbell rang, motion
fired, a vacuum ran, a light switched, a Pico button was pressed, Alexa
heard a command. Only `ts`/`source`/`device`/`event` are required.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time of the event |
| `source` | string | ✔ | collector id, = the folder name |
| `device` | string | ✔ | device id or name the event came from |
| `event` | string | ✔ | what happened, lower-case verb/noun: `unlock`, `lock`, `motion`, `doorbell`, `run`, `on`, `off`, `command`, `open`, `close`, `leak`, … |
| `value` | number | | a numeric payload where one fits (brightness after `on`, area cleaned) |
| `detail` | string | | free-form payload: the method (`"keypad"`), the cleaned area, the transcribed Alexa command |
| `who` | string | | raw handle of who triggered it (keypad code id, account handle) — never resolved to a person |
| `who_name` | string | | display name for `who`, if the source gives one |
| `guid` | string | | source-unique event id, the dedupe key |
| `extra` | object | | everything source-specific (response text, event subtype, clip URL, …) |

## The energy interval — `home/<source>/energy/YYYY-MM.jsonl`

Metered flow over a time bucket beginning at `ts`: grid/solar electricity,
gas, or water. Electricity — the overwhelmingly common case — fills `kwh`;
other commodities use the universal `value` + `unit` pair. Only
`ts`/`source` are required, so a bare hourly-kWh export writes a three-field
line.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the interval **starts** |
| `source` | string | ✔ | collector id, = the folder name |
| `device` | string | | site/meter/monitor id or name |
| `circuit` | string | | sub-channel within the device: a circuit (Emporia), a plug, a commodity meter (`"gas-meter"`) |
| `kwh` | number | | electricity for the interval (the named common case) |
| `value` | number | | quantity when the commodity isn't electricity (gas, water) |
| `unit` | string | | unit of `value`: `therm`, `ccf`, `gal`, `m3`, `wh` (omit when `kwh` carries it) |
| `interval_secs` | int | | length of the bucket in seconds (3600 = hourly, 900 = 15-min) |
| `direction` | string | | `"consumption"` \| `"production"` \| `"import"` \| `"export"` |
| `guid` | string | | source-unique id, the dedupe key |
| `extra` | object | | everything source-specific (cost estimate, voltage, tariff, …) |

## Examples

```jsonl
{"ts":"2026-06-10T14:05:00-07:00","source":"airthings","metric":"radon","value":48,"unit":"bq_m3","place":"Bedroom","device":"2960123456","extra":{"battery":86}}
{"ts":"2026-06-10T14:05:00-07:00","source":"awair","metric":"co2","value":812,"unit":"ppm"}
{"ts":"2026-06-10T14:04:48-07:00","source":"weatherflow-tempest","metric":"temperature","value":21.4,"unit":"C","place":"Backyard","device":"ST-00012345","lat":37.7793,"lon":-122.4193}
```

```jsonl-event
{"ts":"2026-06-10T18:42:11-07:00","source":"august-yale","device":"front-door","event":"unlock","detail":"keypad","who":"guest-code-3","extra":{"eventId":"a1b2c3"}}
{"ts":"2026-06-10T18:41:55-07:00","source":"ring","device":"Front Doorbell","event":"motion","guid":"6612f0a1-7c3d-4e2a-9b11-0f2e4c8d1a55"}
{"ts":"2026-06-10T07:15:03-07:00","source":"amazon-alexa","device":"Kitchen Echo","event":"command","detail":"set a timer for ten minutes","extra":{"response":"Ten minutes, starting now."}}
```

```jsonl-energy
{"ts":"2026-06-10T13:00:00-07:00","source":"tesla-energy","device":"site-228841","kwh":2.74,"interval_secs":3600,"direction":"production"}
{"ts":"2026-06-10T13:00:00-07:00","source":"green-button","kwh":0.91,"interval_secs":3600,"direction":"consumption","guid":"meter-77100-2026061013"}
{"ts":"2026-06-10T13:00:00-07:00","source":"green-button","circuit":"gas-meter","value":0.42,"unit":"therm","interval_secs":3600,"direction":"consumption"}
{"ts":"2026-06-10T13:05:00-07:00","source":"emporia","circuit":"Dryer","kwh":0.013}
```

## Read-time semantics (FYI for writers)

Readers scan `home/*/` for readings, `home/*/events/` for events, and
`home/*/energy/` for intervals; creating the folder is the registration.
Home readings and `environment/` readings are charted together on `metric` —
keep `metric` names snake_case and stable so an indoor `temperature` and an
outdoor one line up. Devices in this domain almost never expose history, so
gaps while the app or daemon is asleep are expected and honest — never
invent samples to fill them. Two sources observing one device (a Hue light
seen directly and again through Home Assistant) each write their own folder
with their own ids; de-duplication and precedence are a read-time opinion,
never a write-time merge. Config-only sources that aren't reading-, event-,
or interval-shaped (HomeKit's home topology) stay per-source raw and don't
use these shapes.
