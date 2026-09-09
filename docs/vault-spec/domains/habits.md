# Domain: habits

Personal routines the user tracks deliberately — and the per-day record of
whether each one happened. Habitica (habits + dailies), Habitify, and TickTick's
habits feature write this shape over their APIs; Way of Life writes it from a
CSV/Excel export; a 20-line script in front of any habit app with an export gets
the same first-class treatment. The store splits like `tasks/`: a **habit**
snapshot (the current definitions, rewritten whole) plus an append-only
**check-in** stream (one row per habit-day). Readers see one set of habits and
one check-in history regardless of which app produced them; cross-source overlap
(the same routine logged in two apps) is reconciled at read time — each source
keeps its own folder and stable ids.

- **Layout:** `habits/<source>/habits.jsonl` (snapshot) +
  `habits/<source>/checkins/YYYY-MM.jsonl` (event stream, month of `date`)
- **Kind:** snapshot + events
- **Schemas:**
  [`schemas/habits.habit.schema.json`](../schemas/habits.habit.schema.json),
  [`schemas/habits.checkin.schema.json`](../schemas/habits.checkin.schema.json)
- **Dedupe key:** check-ins dedupe on `source` + `habit` + `date` (one row per
  habit per day); `guid` carries the source-native id when there is one. Imports
  must skip already-stored habit-days. The snapshot is keyed by `id`.

This is **not** `tasks/`: TickTick's and Habitica's to-dos are tasks (the ratified
`tasks` contract); only their habit/daily *streaks* land here. A habit's full raw
payload — RPG progression, reminders, the per-day history blob — stays at full
fidelity in `habits/<source>/raw/`; only the definition and the normalized
check-ins join the contract.

## The snapshot — `habits.jsonl`

The **current habits**, one per line, rewritten whole (atomically: sibling tmp +
rename) on every sync. Only `source`/`id`/`title` are required — a yes/no tracker
like Way of Life writes just those three; a measurable habit fills `goal`+`unit`.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `source` | string | ✔ | collector id, = the folder name |
| `id` | string | ✔ | source-native habit id (Way of Life: a stable hash of the habit name) |
| `title` | string | ✔ | habit name |
| `schedule` | string | | cadence, verbatim from the source: an RRULE, `"daily"`, `"weekly"`, `"Mon,Wed,Fri"` — raw, not interpreted |
| `goal` | number | | per-period target for a measurable habit (`8` glasses, `30` minutes) |
| `unit` | string | | the unit `goal` is counted in (`"Glass"`, `"min"`, `"page"`) |
| `color` | string | | display color, where the source has one |
| `archived` | bool | | habit is archived / paused / no longer active |
| `created`, `modified` | string | | RFC3339 local |
| `extra` | object | | everything source-specific (streak counts, RPG XP/level/gold, reminders, icon, target days, …) |

## The check-in stream — `checkins/YYYY-MM.jsonl`

Append-only. One line per **habit-day**: the day the habit was marked and how it
went. Only `date`/`source`/`habit`/`status` are required — a yes/no tracker writes
four fields; a measurable habit adds `value`. `date` (not `ts`) is the key because
the calendar day is the one thing every source agrees on — Way of Life records no
time of day. A source that knows the exact moment adds `ts`.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `date` | string | ✔ | the day checked, `YYYY-MM-DD` (local) |
| `source` | string | ✔ | collector id, = the folder name |
| `habit` | string | ✔ | the habit `id` this check-in belongs to |
| `status` | string | ✔ | `"done"` \| `"skipped"` (deliberately excused — Way of Life's skip, a vacation day) \| `"missed"` |
| `ts` | string | | RFC3339 local moment of the check-in, when the source records one |
| `value` | number | | logged amount for a measurable habit (`8` glasses, `30` minutes) — the per-day total |
| `note` | string | | free-form note attached to the check-in |
| `guid` | string | | source-native check-in id, where one exists |
| `extra` | object | | everything source-specific (goal-at-the-time, raw stamp, mood, …) |

Status is a coarse normalization of each app's own vocabulary (TickTick's
`0`/`1`/`2`, Habitica's per-day history values, Way of Life's yes/no/skip); the raw
flag stays in `extra`. Don't invent a fate you can't observe: a measurable habit
that logged a partial amount is still `"done"` unless the source says otherwise —
`value` carries the shortfall, `status` is not guessed from it. A habit-day the
source simply never recorded is **absent**, not a `"missed"` row.

## Examples

```jsonl
{"source":"ticktick","id":"6247e8f0b3a1c2","title":"Drink water","schedule":"RRULE:FREQ=DAILY;INTERVAL=1","goal":8,"unit":"Glass","color":"#97E38B","archived":false,"created":"2026-01-04T08:00:00-08:00","extra":{"totalCheckIns":118,"targetDays":21}}
{"source":"habitica","id":"a1f4c0de-2b77-4f3e-9c11-8d6e0f5a2b34","title":"Meditate","schedule":"weekly","archived":true,"extra":{"frequency":"daily","streak":0,"history_len":204,"xp_reward":true}}
{"source":"way-of-life","id":"f3a9c1b27e","title":"No alcohol"}
```

```jsonl-checkin
{"date":"2026-06-10","source":"ticktick","habit":"6247e8f0b3a1c2","status":"done","ts":"2026-06-10T21:14:00-07:00","value":8,"extra":{"goal":8,"stamp":20260610}}
{"date":"2026-06-10","source":"way-of-life","habit":"f3a9c1b27e","status":"skipped"}
{"date":"2026-06-09","source":"habitica","habit":"a1f4c0de-2b77-4f3e-9c11-8d6e0f5a2b34","status":"missed","note":"travel day"}
```

## Read-time semantics (FYI for writers)

The habits reader scans `habits/*/habits.jsonl` for the current habit set and
`habits/*/checkins/*.jsonl` for streaks and completion rates; creating those
folders is the registration. Streaks and completion percentages are derived at
read time from the check-in stream joined to the snapshot on `habit` — never
persist a computed streak back into the vault (a habit's `extra.streak` is the
source's own number, kept for provenance, not Trove's). Cross-source dedupe (the
same routine tracked in two apps) is a read-time opinion — write your own folder
with stable ids and let the reader reconcile. The snapshot is backfillable from
the API on every sync; the check-in log is the part that often **can't** be
backfilled (Habitica averages and discards older history), so connect early and
append every dated check-in you can still see.
