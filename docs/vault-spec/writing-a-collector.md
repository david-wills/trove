# Writing a Collector (in any language)

A collector is any program that writes correctly-formatted files into the
vault — the "M6 agent collector" mechanism. No SDK, no registration: if the
files follow [`conventions.md`](conventions.md) and the relevant
[domain spec](domains/), the data appears in Trove's UI on the next read.

## A complete example

This Python script is a whole, working collector: it maintains one source
(`my-todo-script`) in the normalized [tasks contract](domains/tasks.md).
Run it, open Trove's Tasks tab, and the task is there.

```python
#!/usr/bin/env python3
"""A complete Trove collector: one open task + one completion event."""
import datetime
import json
import os
import pathlib

VAULT = pathlib.Path.home() / "Trove"
SOURCE = "my-todo-script"  # lowercase/digits/dash; also the folder name
now = datetime.datetime.now().astimezone().isoformat(timespec="seconds")

base = VAULT / "tasks" / SOURCE
(base / "events").mkdir(parents=True, exist_ok=True)

# 1. The open-task snapshot: rewritten whole, atomically (tmp + rename).
tasks = [{"source": SOURCE, "id": "42", "title": "Water the plants"}]
tmp = base / "tasks.jsonl.tmp"
tmp.write_text("".join(json.dumps(t) + "\n" for t in tasks))
os.replace(tmp, base / "tasks.jsonl")

# 2. The event stream: append-only, one JSON object per line, month files.
event = {"time": now, "kind": "completed",
         "source": SOURCE, "id": "41", "title": "Buy seeds", "status": "done"}
with open(base / "events" / f"{now[:7]}.jsonl", "a") as f:
    f.write(json.dumps(event) + "\n")

print(f"wrote {base}")
```

## The checklist

Before calling a collector done, confirm:

- [ ] **Timestamps** are RFC3339 with the local offset (`.astimezone()`
      above); date prefixes sort lexically.
- [ ] **Streams are append-only**, one JSON object per line,
      newline-terminated, partitioned by the record's own day or month.
- [ ] **Snapshots are atomic**: full rewrite to a sibling `.tmp`, then
      rename over the target — never truncate in place.
- [ ] **Re-runnable**: records carry a source-unique `guid`/`id`, and you
      skip what's already on disk before appending. Running twice must not
      duplicate anything.
- [ ] **Own folder only**: you write `domain/<your-source>/…` and nothing
      else. Your source id is lowercase/digits/dash and matches the
      `source` field in every record.
- [ ] **Nothing dropped**: source fields with no normalized column ride in
      `extra`. Full fidelity at write time; opinions belong to readers.
- [ ] **Cursors** (if incremental) live at `.trove/<your-source>-sync.json`
      with an `updated` RFC3339 field — and are rebuildable by scanning
      your own output files.
- [ ] **No secrets in data folders.** Tokens go under `.trove/sync/`,
      0600, atomic write.

## Auth is yours (for now)

External collectors handle their own login: if your source needs OAuth or an
API key, your program runs the flow and stores the token itself (under
`.trove/sync/`, per the checklist). Trove's in-binary OAuth machinery —
PKCE, loopback redirect, refresh, the hub's connect cards — serves only
integrations compiled into Trove (`ConnectionDef` in the registry); there is
deliberately no token-brokering IPC, because handing live tokens to outside
processes is a trust surface we won't open speculatively. If you're building
an external collector that genuinely needs brokered auth, open an issue —
real demand is what would justify designing it.

## Where to write what

| You collected… | Write the… | Spec |
|---|---|---|
| messages, calls, conversations | correspondence contract | [domains/correspondence.md](domains/correspondence.md) |
| to-dos from any task app | tasks contract (snapshot + events) | [domains/tasks.md](domains/tasks.md) |
| listens/watches/plays | media-plays contract | [domains/media-plays.md](domains/media-plays.md) |
| app/window activity spans from a sampler | activity stream, in your own `activity/<source>/` subfolder | [domains/activity.md](domains/activity.md) |
| web page visits | browser-visits stream (multi-writer; take the flock) | [domains/browser-visits.md](domains/browser-visits.md) |
| ads seen while browsing | ads stream | [domains/ads.md](domains/ads.md) |
| something with no contract yet | your own folder of date-partitioned JSONL per [conventions](conventions.md) — it appears in `.trove/manifest.json` and the generic data browser automatically | — |

When a domain you need has no contract yet, open an issue/PR proposing one —
contracts get named once two sources want the same shape.
