# Boards

A board is a user-curated page of charts, stored as one markdown file with
YAML frontmatter at `boards/<slug>.md`. It is the app's merged view by
construction: every panel names its series outright — a catalog metric, or
a table column with an aggregate — so no reader has to decide which of two
sources wins. The `overview` board is the one Health shows first. Boards are files so they can be copied between vaults and
shared; a series that keeps recurring across shared boards is the demand
signal for a designed view.

- **Layout:** `boards/<slug>.md`. `<slug>` is `[a-z0-9-]`, the file's
  identity; the title inside is free text.
- **Kind:** a document the app rewrites whole, atomically, when the user
  edits the board in the window. Hand edits are fine; the app re-reads on
  every open. A file that fails to parse is skipped in the list, never
  deleted.
- **Body:** everything under the closing `---` is markdown notes about the
  board (what question it answers). Optional.

## Frontmatter

| Field | Type | Required | Meaning |
|---|---|---|---|
| `title` | string | ✔ | shown as the board's heading |
| `panels` | list | | panels in display order; a board with none is a note |

### Panel

| Field | Type | Required | Meaning |
|---|---|---|---|
| `title` | string | | panel heading; the first series' label when empty |
| `kind` | string | ✔ | `line` \| `bars` \| `dual` \| `heatmap` \| `gaps` (below) |
| `bucket` | string | | `day` (default) \| `week` \| `month` — the fold granularity |
| `days` | integer | | days back from `to` (default 90) |
| `to` | string | | last day shown, `YYYY-MM-DD`; today when absent, so the panel stays live. Set it to freeze a panel around a date |
| `series` | list | ✔ | one or two series (below); `dual` and `gaps` take exactly two |

### Series

A series is **one of two forms**: a catalog metric, or a table column.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `metric` | string | one form | a metric slug from the health catalog (`sleep-score`, `steps`, `hrv`); read through the app's typed path, so source semantics (relay dedupe, sum-vs-average per metric) apply |
| `source` | string | | with `metric`: one source to read (`oura`, `apple-health`); every source that reports it when empty, each drawn as its own line |
| `table` | string | other form | a table id: a dated stream directory (`calendar/events`, `health/sleep/oura`) or one undated file without its extension (`health/oura/daily_sleep`) |
| `column` | string | with `table` | a numeric field of the table's records, nested one level as `outer.inner` (`contributors.deep_sleep`, `extra.efficiency`); or `@records`, the count of records per day every table has |
| `agg` | string | | `avg` (default, weighted by record count) \| `sum` \| `min` \| `max` \| `count` — how a table column's values fold; ignored for a metric |
| `label` | string | | legend label; the metric or column name when empty |
| `divide` | number | | divide every value by this before plotting (`3600` turns seconds into hours); presentation only |
| `unit` | string | | shown on the axis and in the legend; series with a different unit from the first take a right-hand axis |

A panel holds up to six series.

### Panel kinds

- **`line`** — each series as a line. A day with no value breaks the line,
  so a gap in the data is visible as a gap, never bridged. Series whose
  `unit` differs from the first's are drawn against a right-hand axis.
- **`bars`** — one series as bars.
- **`dual`** — two series on one time axis, the second against a right-hand
  scale (a 0–100 score against a count of events).
- **`heatmap`** — one series as a calendar: one cell per day, shaded by
  value, laid out in weeks. `bucket` is ignored (always day).
- **`gaps`** — the first series as bars, plus a marker on every day in the
  range where the *second* series has no value at all: "missing nights"
  over event bars. The second series is never drawn, only its absence.
- **`tile`** — the latest value of each series as a card with its date:
  an overview number rather than a chart. `days` bounds how far back
  "latest" may look.

A day with no record for a column is **absent, not zero**: the read side
never invents a value, which is what makes gaps honest.

## Example

```markdown
---
title: Sleep × Calendar
panels:
- title: Today
  kind: tile
  bucket: day
  days: 7
  series:
  - metric: readiness-score
    agg: avg
  - metric: sleep-score
    agg: avg
- title: Sleep score vs meetings
  kind: dual
  bucket: day
  days: 90
  series:
  - metric: sleep-score
    source: oura
    agg: avg
    label: Sleep score
  - table: calendar/events
    column: '@records'
    agg: sum
    label: Events
- title: Hours asleep per week
  kind: bars
  bucket: week
  days: 180
  series:
  - table: health/sleep/oura
    column: asleep_seconds
    agg: sum
    divide: 3600.0
    unit: h
---

Does a packed calendar cost sleep?
```

## How a series is read

The app keeps a rebuildable index per table at
`.trove/columns/<table>.json`: per file, per day, the record count and
`(count, sum, min, max)` of every numeric field. A record's day is the
same one the generic stream read uses (`ts`, `occurrence`, `start`,
`day`, `date`, `timestamp`, `time`, first present). A panel costs the
index, not the stream; a file that changes re-indexes alone. Delete the
index folder and it comes back on the next read. Nothing derived is ever
written into the data folders.

Tables are discovered, not registered: any directory of `.jsonl` files a
collector writes (following [conventions](conventions.md)) is chartable
the moment the file lands — including columns under `extra`, which is how
the long tail of a source stays self-serve without anyone mapping it.
