# Domain: time-entries

**User-asserted** time entries — what the owner *says* they spent time on,
logged by hand against projects, clients, and tasks in a manual time tracker.
Toggl Track, Clockify, and Harvest write this shape (Timery is a Toggl
frontend with no store of its own, so it lands here under `toggl-track`);
readers see one stream of how the owner accounted for their hours, regardless
of tracker. This is the deliberate counterpart to `activity/` — **observed**
auto-trackers (RescueTime, Timing) are passive measurements and write
`activity/<source>/`, never here. The two merge only at read time, where a
view can compare asserted hours against observed ones.

- **Layout:** `time-entries/<source>/YYYY-MM.jsonl` (month of `start`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/time-entries.entry.schema.json`](../schemas/time-entries.entry.schema.json)
- **Dedupe key:** `id` (source-native entry id, source-unique). Re-runnable
  imports skip already-stored ids before appending. The stream is append-only,
  so the **first** observed state of an `id` is the one that lands: an entry
  first seen while its timer is still running is written with no
  `end`/`duration_secs`, and a later poll that catches it stopped is dropped at
  the contract layer (the `id` is already held) — the entry's final
  `end`/`duration_secs` survive only in the per-source `raw/` snapshot, not in
  the normalized row. (To avoid open rows entirely a collector can let an
  in-progress timer settle and emit it once stopped; raw/ keeps every snapshot
  regardless.)

Source folders are discovered by scanning — no registration, no code change.

## Entry

One record per time entry. Only `source`, `id`, and `start` are required —
they place and identify the entry; everything else is omit-if-empty, so a
sparse free-tier row carries just a description while a rich one fills
project/client/task/tags/billable. A **running timer** is an entry with a
`start` and no `end`/`duration_secs` (both omitted until it stops).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `source` | string | ✔ | collector id, = the folder name |
| `id` | string | ✔ | source-native entry id, the dedupe key |
| `start` | string | ✔ | when the entry began: RFC3339 local time, **or** a date-only `YYYY-MM-DD` for a duration-only entry the source never timestamped (Harvest "X hours on this day") |
| `end` | string | | RFC3339 local time the entry stopped; omitted while a timer runs |
| `duration_secs` | int | | seconds tracked; omitted while a timer runs. Present for duration-only entries that have no `end` |
| `description` | string | | the user's note for the entry |
| `project` | string | | project name, verbatim |
| `client` | string | | client name, verbatim (Harvest/Clockify; absent on trackers without clients) |
| `task` | string | | task/sub-activity name within the project, verbatim |
| `tags` | string[] | | source-native tag labels |
| `billable` | bool | | whether the entry is marked billable |
| `extra` | object | | everything source-specific (workspace/account id, hourly rate, rounding, invoice id, color, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"source":"toggl-track","id":"3691827456","start":"2026-06-10T09:00:00-07:00","end":"2026-06-10T10:30:00-07:00","duration_secs":5400,"description":"Quarterly traffic report","project":"Editorial","tags":["deep-work"],"billable":true,"extra":{"workspace_id":1234567}}
{"source":"clockify","id":"657f1a9b2c3d4e5f6a7b8c9d","start":"2026-06-11T13:15:00-07:00","description":"Pairing on the import parser","project":"Trove","task":"time-entries collector"}
{"source":"harvest","id":"636709355","start":"2026-06-09","duration_secs":7740,"description":"On-site client workshop","project":"Website Redesign","client":"Acme Co","billable":true}
```

## Read-time semantics (FYI for writers)

The time-entries reader scans `time-entries/*/`; creating your source folder
is the registration. Duration is taken from `duration_secs` when present, else
computed as `end − start`; an entry with only a `start` is a running timer and
contributes no closed duration. Totals group by `project` (then `client`,
where present) within a date range. Don't synthesize a clock time you can't
observe — a Harvest duration-only entry keeps its date-only `start` rather
than a fabricated midnight, and the reader buckets it by day. Per-source raw
fidelity lives under `time-entries/<source>/raw/`; this contract is the
normalized convergence, not a superset of every tracker's fields.
