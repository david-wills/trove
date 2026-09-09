# Domain: tasks

To-dos from any task app, in one normalized store. TickTick, Apple
Reminders, and Google Tasks write this shape; so can a 20-line script in
front of any app with an export (that's the point — even a dumb periodic
export-converter gets first-class treatment in the app).

- **Layout:** `tasks/<source>/tasks.jsonl` (snapshot) +
  `tasks/<source>/events/YYYY-MM.jsonl` (event stream, month of `time`)
- **Kind:** snapshot + events
- **Schemas:** [`schemas/tasks.task.schema.json`](../schemas/tasks.task.schema.json),
  [`schemas/tasks.event.schema.json`](../schemas/tasks.event.schema.json)

## The snapshot — `tasks.jsonl`

The **current open tasks**, one per line, rewritten whole (atomically:
sibling tmp + rename) on every sync. Only `source`/`id`/`title` are
required — sparse sources write minimal lines.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `source` | string | ✔ | collector id, = the folder name |
| `id` | string | ✔ | source-native task id |
| `title` | string | ✔ | |
| `project` | string | | project/list name, verbatim |
| `notes` | string | | free-form body |
| `status` | string | | `"open"` (default) \| `"done"` |
| `priority` | int | | TickTick scale: 0 none, 1 low, 3 medium, 5 high — map yours in |
| `due`, `start` | string | | RFC3339 local |
| `all_day` | bool | | |
| `recurrence` | string | | raw RRULE |
| `tags` | string[] | | |
| `subtasks` | object[] | | `{title, done, completed?}` |
| `created`, `modified`, `completed` | string | | RFC3339 local; for a recurring task `completed` is the last instance completion |
| `extra` | object | | everything source-specific (full fidelity) |

## The event stream — `events/YYYY-MM.jsonl`

Append-only. One line per task lifecycle event: the full task shape
(flattened, same fields as above) plus:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `time` | string | ✔ | RFC3339 local time of the event |
| `kind` | string | ✔ | `"completed"` \| `"created"` \| `"deleted"` |

This stream is the part that often **cannot be backfilled** (many APIs only
expose open tasks) — completions exist because each sync diffs the new
snapshot against the previous one. Never guess a fate: if you can't tell
completed from deleted, carry the task forward and retry next run.

## Examples

```jsonl
{"source":"ticktick","id":"6f1a","title":"Renew passport","project":"Errands","status":"open","priority":5,"due":"2026-07-01T09:00:00-07:00","tags":["admin"],"created":"2026-06-01T10:00:00-07:00","extra":{"projectId":"inbox123"}}
{"source":"my-todo-script","id":"42","title":"Water the plants"}
```

```jsonl-events
{"time":"2026-06-10T17:30:00-07:00","kind":"completed","source":"ticktick","id":"6f1a","title":"Renew passport","project":"Errands","status":"done","completed":"2026-06-10T17:29:12-07:00"}
```

Readers scan `tasks/*/` — creating your source folder is the registration.
An optional human layer (`<project>.md` checklists + `index.md`) may sit
alongside; Trove regenerates its own, and ignores yours.
