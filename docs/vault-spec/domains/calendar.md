# Domain: calendar

Every event from every synced calendar, in one store. Trove's built-in
collector reads them all in one shot via Apple Calendar / EventKit (iCloud,
Google, Workspace, Exchange, subscriptions); an external collector — an ICS
fetcher, a CalDAV script, a one-off export converter — writes the same shape
and merges at read time.

- **Layout:** `calendar/events/YYYY-MM.jsonl` (occurrence snapshot, month of
  `start`) + `calendar/changes/YYYY-MM.jsonl` (diff stream, month observed)
- **Kind:** snapshot + events
- **Schemas:**
  [`schemas/calendar.occurrence.schema.json`](../schemas/calendar.occurrence.schema.json),
  [`schemas/calendar.change.schema.json`](../schemas/calendar.change.schema.json)
- **Occurrence key:** `id` + `occurrence` (one recurring event shares an `id`
  across all its instances; `occurrence` is the instance's original slot).

## The snapshot — `events/YYYY-MM.jsonl`

The **current state** of the calendar: one occurrence per line, recurrences
expanded, partitioned by the month of `start`. Each sync rewrites only the
months whose content changed (atomically: sibling tmp + rename); months
older than the sync window freeze and are never rewritten. Only `id` is
required — everything else is omit-empty.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | ✔ | event identifier, shared across all occurrences of a recurring event |
| `occurrence` | string | | the instance's original slot (RFC3339 local); stable across a reschedule, which is what makes `id`+`occurrence` a usable key |
| `start`, `end` | string | | RFC3339 local; for all-day events, local midnight → `23:59:59` |
| `all_day` | bool | | |
| `title` | string | | |
| `calendar` | string | | calendar name (`"Work"`, `"Birthdays"`, …) |
| `account` | string | | account the calendar syncs from (`"iCloud"`, `"dwills@example.com"`, …) |
| `location` | string | | |
| `notes` | string | | free-form body, verbatim (may contain HTML) |
| `attendees` | string[] | | |
| `status` | string | | `"confirmed"` \| `"tentative"` \| `"canceled"` \| `""` (none reported) |
| `recurring` | bool | | whether this occurrence belongs to a recurring series |

## The change stream — `changes/YYYY-MM.jsonl`

Append-only, partitioned by the month a sync **observed** the change. This
is the **unrecoverable** layer: the snapshot always reflects the present, so
reschedule and cancellation history exists only because each sync diffs the
fresh occurrences against the stored ones. The occurrences themselves are
fully backfillable; this stream is not.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time of the sync that observed the change |
| `kind` | string | ✔ | `"added"` \| `"changed"` \| `"removed"` |
| `id` | string | ✔ | event id |
| `occurrence` | string | | instance slot, as in the snapshot |
| `title`, `start`, `calendar` | string | | a thumbnail of the event, for context without a snapshot join |
| `changes` | object[] | | only on `"changed"`: exactly the fields that drifted, each `{field, old, new}` (`old`/`new` are the raw JSON values) |

The **first-ever sync writes no `added` lines** — years of existing events
are state, not events. A change beyond the sync window's edge (e.g. a far-future
event moved further out) logs as `removed` (a documented trade-off).

## Examples

```jsonl
{"id":"AB2C4FD5-1486-407E-97E6-21CECAA1D03E","occurrence":"2026-03-02T10:00:00-08:00","start":"2026-03-02T10:00:00-08:00","end":"2026-03-02T11:00:00-08:00","all_day":false,"title":"Monday Traffic Brainstorm","calendar":"Work","account":"dwills@example.com","location":"Google Meet","attendees":["dwills@example.com","sam@example.com"],"status":"confirmed","recurring":true}
{"id":"8256D685-EC6F-4E61-A4B3-30AF828896DA","occurrence":"2026-05-03T00:00:00-07:00","start":"2026-05-03T00:00:00-07:00","end":"2026-05-03T23:59:59-07:00","all_day":true,"title":"Matt's Birthday","calendar":"Birthdays","account":"iCloud","recurring":true}
```

```jsonl-changes
{"ts":"2026-06-11T14:56:49-07:00","kind":"added","id":"F1834855-069B-4613-A0A0-9581338FBFA1","occurrence":"2026-06-20T19:15:00-07:00","title":"Get Haircut","start":"2026-06-20T19:15:00-07:00","calendar":"TickTick"}
{"ts":"2026-06-11T15:02:10-07:00","kind":"changed","id":"AB2C4FD5-1486-407E-97E6-21CECAA1D03E","occurrence":"2026-03-02T10:00:00-08:00","title":"Monday Traffic Brainstorm","start":"2026-03-02T11:00:00-08:00","calendar":"Work","changes":[{"field":"start","old":"2026-03-02T10:00:00-08:00","new":"2026-03-02T11:00:00-08:00"}]}
{"ts":"2026-06-11T14:12:04-07:00","kind":"removed","id":"F1834855-069B-4613-A0A0-9581338FBFA1","occurrence":"2027-06-01T19:15:00-07:00","title":"Schedule Haircut","start":"2027-06-01T19:15:00-07:00","calendar":"TickTick"}
```

## Read-time semantics (FYI for writers)

The calendar reader scans `events/*.jsonl` for timelines and summaries and
`changes/*.jsonl` for history; creating those folders is the registration.
Don't invent a fate you can't observe — if a sync can't tell a cancellation
from an event leaving the window, `removed` is the honest call. Write full
RFC3339-local timestamps; never persist a derived view back into the vault.
