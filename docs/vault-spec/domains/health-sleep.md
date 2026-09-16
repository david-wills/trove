# Domain: health-sleep

Sleep as **sessions**: one record per night, nap, or rest, as one source
observed it. Oura writes its sessions here from the ring; Apple Health
writes the sessions it holds — which, because Apple Health is itself a
relay, may come from an Apple Watch, the Oura app, AutoSleep, Pillow, a
Withings mat, or a bedtime alarm, each marked by `origin`. This is the
normalized convergence of every sleep tracker, not a superset of them: a
session is when it started and ended, how long was spent asleep, and the
stage totals where the source has them. Everything else a source knows
about a night rides under `extra` (scalars) or stays in the source's raw
file (per-sample arrays), joinable by `guid`. This is the first contract
where the same night reaches the vault twice by design — Oura directly and
Oura again through Apple Health — and it is written that way on purpose:
each writer records what it observed, and the read side decides which one
to show.

- **Layout:** `health/sleep/<source>/YYYY-MM.jsonl` (month of `day`). The
  Apple Health importer's per-stage CSVs also live under `health/sleep/`
  as files at the root (`YYYY-MM.csv`, `daily.csv`; see
  [health](health.md)); readers of this contract scan **subdirectories**
  only, so the two coexist.
- **Kind:** per-source projection stream. A writer that holds its source's
  full-fidelity raw records (Oura's `health/oura/sleep.jsonl`; an Apple
  Health `export.zip`) regenerates its own month files whole from raw,
  atomically, because both of these sources revise a session after the
  fact (Oura re-scores; a re-imported export replaces). A writer without
  raw appends with `guid` dedupe like any event stream. Either way a
  writer touches only its own `<source>/` folder.
- **Schema:** [`schemas/health-sleep.session.schema.json`](../schemas/health-sleep.session.schema.json)
- **Dedupe key:** `guid` — the source's session id where it has one
  (Oura's sleep `id`); for a relay, `<origin-slug>:<start>`, which is
  stable across re-imports of the same export.

## Session

One session per line. `day`, `start`, `end`, `source`, `guid` are
required; everything else is omit-if-empty. `day` is the date the session
belongs to **as the source attributes it**: for a night, the morning you
woke up, so a night that crosses midnight lands on one day and a missing
night is a `day` with no row, which a chart can show as a gap rather than
a zero. A source with its own sleep-day boundary keeps it — Oura's day
turns over at 18:00, so an evening nap belongs to the *next* day, and
that is the `day` its daily sleep score is keyed by. A writer whose source
has no such notion uses the local date of `end`. Readers group by `day`,
never by the date of `end`. Durations are whole seconds so a reader never
parses timestamps to total a week.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `day` | string | ✔ | `YYYY-MM-DD` the session belongs to, as the source attributes it (Oura's `day`; otherwise the local date of `end`); the partition key's day |
| `start` | string | ✔ | RFC3339 local time the session began |
| `end` | string | ✔ | RFC3339 local time the session ended |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id; the dedupe key and the join key into the raw file |
| `origin` | string | | the app or device that recorded the session when the writer is a relay (Apple Health's `sourceName`); omitted when the writer is the device |
| `kind` | string | | `"sleep"` \| `"nap"` \| `"rest"` — the source's own classification, where it has one (Oura: `long_sleep` → sleep, `sleep` / `late_nap` → nap, `rest` → rest); omitted when the source does not classify |
| `asleep_seconds` | integer | | total time asleep, every stage summed, awake time excluded |
| `in_bed_seconds` | integer | | time in bed, `start` to `end`, awake time included |
| `deep_seconds` | integer | | deep (slow-wave) sleep, where the source reports stages |
| `rem_seconds` | integer | | REM sleep, where the source reports stages |
| `light_seconds` | integer | | light sleep, where the source reports stages (Apple's "Core" stage) |
| `awake_seconds` | integer | | time awake inside the session, where the source reports it |
| `extra` | object | | the source's scalar fields the shape has no column for (Oura efficiency, latency, average_hrv, lowest_heart_rate, average_breath, restless_periods, …). Per-sample arrays stay in the raw file |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"day":"2026-06-10","start":"2026-06-09T23:12:40-07:00","end":"2026-06-10T07:02:15-07:00","source":"oura","guid":"5b1f0e6a-3c2d-4e8f-9a11-7d2c0b4e9f31","kind":"sleep","asleep_seconds":25380,"in_bed_seconds":28175,"deep_seconds":4980,"rem_seconds":6120,"light_seconds":14280,"awake_seconds":2795,"extra":{"efficiency":90,"latency":420,"average_hrv":54,"average_heart_rate":56.2,"lowest_heart_rate":49,"average_breath":14.8,"restless_periods":212,"sleep_algorithm_version":"v2"}}
{"day":"2026-06-10","start":"2026-06-10T14:30:00-07:00","end":"2026-06-10T15:05:00-07:00","source":"oura","guid":"9c7a2d10-1b3e-4f55-8e02-a4d6f0c1b2e3","kind":"nap","asleep_seconds":1740,"in_bed_seconds":2100,"awake_seconds":360,"extra":{"efficiency":83}}
{"day":"2026-06-10","start":"2026-06-09T23:05:00-07:00","end":"2026-06-10T06:58:00-07:00","source":"apple-health","guid":"apple-watch:2026-06-09T23:05:00-07:00","origin":"Apple Watch","asleep_seconds":25200,"in_bed_seconds":28380,"deep_seconds":3900,"rem_seconds":5700,"light_seconds":15600,"awake_seconds":3180}
```

## Read-time semantics (FYI for writers)

Readers scan `health/sleep/*/`; creating your source folder is the
registration. A night view groups sessions by `day`; a duration series
sums `asleep_seconds` per `day` (or per week, averaging nights); stage
charts read the `*_seconds` columns and skip sessions without them.

**Precedence is a read-time opinion, and the default is: the device beats
the relay.** When a relay row's `origin` names a source that also writes
its own folder here — Apple Health's `"Oura"` rows next to `oura/` — the
direct row is shown and the relay row is kept but hidden, so a vault with
both never double-counts a night. Two different origins on the same night
(an Apple Watch and an Oura ring, both through Apple Health) are two
observations of one night and are shown per origin, never merged into
one: the read side can prefer one per metric (a pinned board picks the
winner), but it does not average them. Nothing derived is written back.

Write the session as the source reports it. Do not stitch a relay's
interval rows into sessions differently per import — the Apple Health
importer's rule (intervals from one origin that are less than an hour
apart form one session) is part of this contract so that guids stay
stable. Do not infer `kind` for a source that does not classify; a
reader can call a 40-minute session a nap on its own.
