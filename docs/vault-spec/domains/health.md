# Domain: health (raw layer)

The body's measurements, kept in each source's **own shape**: Apple Health
as one CSV per metric per month, Oura as the ring's API records verbatim.
This page documents the raw layer. The contract layers that sit on top of
it — [health-sleep](health-sleep.md) for sessions, [health-nutrition](health-nutrition.md)
for food logs, [health-medical](health-medical.md) for clinical records —
each have their own page and their own `health/<domain>/<source>/` folders.
Nothing here is normalized across sources at write time: the metric
catalog that shows Apple and Oura heart-rate variability side by side is
built at read time from the two layouts below, and the two never share a
file.

Both of these writers are Trove's own, and their layouts are stable under
the additive-evolution rule, but they are *not* multi-source contracts: a
third sleep tracker does not write Apple's CSV shape, it writes the sleep
contract. What a new health source should write is, in order: the domain
contract its records fit (sleep, nutrition, medical); otherwise its own
raw files under `health/<source>/`, with a note in this spec.

## Apple Health export — `health/<metric>/`

Written by the `export.zip` importer. The export is streamed, never
extracted; every `Record` and `Workout` element lands in a per-metric
folder named by a kebab-case slug of the HealthKit type
(`HKQuantityTypeIdentifierHeartRate` → `heart-rate`; a type the importer
has no name for is slugged mechanically). A re-import **replaces**: every
file for every metric present in the new export is rewritten, so
importing a newer export never duplicates.

- `health/<metric>/YYYY-MM.csv` — raw records, month of `end`. Header
  `start,end,value,unit,source`. `start`/`end` are the export's own
  timestamp format, `YYYY-MM-DD HH:MM:SS ±HHMM` (a recorded exception to
  the RFC3339 convention: the raw layer keeps the export's bytes; the
  contract layer above it is RFC3339). `source` is Apple's `sourceName`
  — the app or device that wrote the sample to HealthKit — and it matters:
  Apple Health is a relay, so one metric folder holds samples from an
  Apple Watch, an iPhone, and every third-party app that syncs in.
  - Quantity types: `value` is the number, `unit` Apple's unit string.
  - `sleep/`: `value` is the stage (`InBed`, `Awake`, `AsleepCore`,
    `AsleepDeep`, `AsleepREM`, `AsleepUnspecified`), `unit` empty. One row
    per interval, and intervals from different origins overlap freely.
    The sleep contract's Apple writer is built from these rows.
  - Other category types: `value` is the category value with its
    `HKCategoryValue` prefix stripped.
- `health/workouts/YYYY-MM.csv` — one row per `Workout`, header
  `start,end,type,duration_min,energy_kcal,distance_km,source`.
- `health/<metric>/daily.csv` — per-day aggregate of the raw rows, header
  `date,count,sum,min,max,avg`, rewritten whole on import. A record is
  attributed to the local date of its `end` (a night crossing midnight
  counts toward the morning). This is the level every chart reads.
- `health/index.md` — the import's human-readable table (metric, folder,
  unit, records, first and last date).
- `.trove/health-summary.json` — the same table as a rebuildable index,
  plus each metric's aggregation kind (`sum` for counts like steps, `avg`
  for readings like heart rate, `sum-then-avg` for daily totals that
  average across days, like sleep hours).

## Oura — `health/oura/`

Written by the Oura sync (OAuth, hourly while the app is open). Every
collection of the Oura API v2 is stored **verbatim**, one JSON record per
line — no field is renamed, dropped, or reshaped. Files are snapshots
sorted by key and rewritten atomically as records arrive or are revised
(Oura re-scores a night after the fact), so a file is never append-only
and a reader should not assume arrival order.

- `health/oura/<collection>.jsonl` — `daily_activity`, `daily_readiness`,
  `daily_sleep`, `daily_spo2`, `daily_stress`, `daily_resilience`,
  `daily_cardiovascular_age`, `vo2_max`, `sleep_time` (keyed by `day`);
  `sleep`, `workout`, `session`, `enhanced_tag`, `rest_mode_period`
  (keyed by `id`). Per-sample arrays (the 5-minute sleep phases,
  heart-rate and HRV samples inside a `sleep` record) are kept in full.
- `health/oura/heartrate/YYYY-MM.jsonl` — continuous heart rate, keyed by
  `timestamp`, month-partitioned because it runs to ~300 records a day.
- `health/oura/personal_info.json`, `ring_configuration.json` — small
  un-ranged payloads, overwritten each sync.
- `health/oura/index.md` — collections, record counts, synced-through
  dates, backfill state.
- `.trove/oura-sync.json` — the sync cursor per collection (watermark and
  backfill cursor), rebuildable by scanning the files.
- `.trove/oura-summary.json` — per-day `(count, sum)` rollups per
  collection, the rebuildable index the metric catalog and every Oura
  series read from, so opening the Health tab costs the index, not the
  vault.

Field meanings are Oura's: see the
[Oura API v2 reference](https://cloud.ouraring.com/v2/docs). A collection
the connected account did not grant a scope for stays empty and is noted
in the sync state; reconnecting with every permission checked fills it.

## Read-time semantics (FYI for writers)

The health reader unifies at read time and only there. The metric catalog
maps both layouts onto one slug per measurement (Apple `hrv` and Oura
`average_hrv` both land under `hrv`) and serves **separate per-source
series**, never a merged line: Apple's HRV is SDNN, Oura's is rMSSD, and a
reader shows the caveat rather than averaging the two. Because Apple
Health relays other apps, an Oura night can be present twice — in
`health/oura/sleep.jsonl` and, via the Oura app, as `Oura`-sourced rows
in `health/sleep/*.csv`; the sleep contract's precedence rule (the
device beats the relay) is how a view shows it once. Nothing derived is
persisted; a changed opinion re-reads the same files.
