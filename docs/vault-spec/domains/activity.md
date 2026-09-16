# Domain: activity

What the owner was doing on their Mac, as **spans**: one record per
contiguous stretch of using a single app and window (or of being away from
the keyboard). This is the observed, sampled view of a day — the
counterpart of the *tracked* view in [time-entries](time-entries.md), where
the owner says what they were doing. The stream is written by an
always-on sampler (Trove's is the separate `trove-collector` program) that
polls the frontmost window and the idle time every few seconds and merges
consecutive samples of the same app + title into one span. Reads aggregate
spans on the fly: per-app totals, active vs. away, a day's timeline.

- **Layout:** `activity/YYYY-MM-DD.jsonl` (day of `start`) — **the live
  sampler's owned stream, single-writer**. An imported observed-span
  history from another tool (ActivityWatch, RescueTime, …) writes its own
  subfolder, `activity/<source>/YYYY-MM-DD.jsonl`, with the same record
  shape; it never appends to the root day files.
- **Kind:** append-only event stream
- **Schema:** [`schemas/activity.event.schema.json`](../schemas/activity.event.schema.json)
- **Dedupe key:** none — a span is unique by `start`; a sampler writes each
  span exactly once, when it closes.

## Event

One span per line. `start`, `end`, `seconds`, `app`, and `afk` are required.
`seconds` is the span's wall-clock length and is stored (not derived) so a
reader never has to parse timestamps to total a day. AFK spans carry
`afk: true` and an empty `app`, so per-app totals never count idle time;
the sampler back-dates an AFK span to the last input, not to the moment it
noticed the idleness. `title` is the window title, empty when the sampler
lacks Screen Recording (macOS hides other apps' titles without it); the
app name works regardless.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `start` | string | ✔ | RFC3339 local time the span began |
| `end` | string | ✔ | RFC3339 local time the span ended |
| `seconds` | integer | ✔ | `end - start` in whole seconds |
| `app` | string | ✔ | the frontmost app's display name; empty for AFK spans |
| `bundle_id` | string | | the app's bundle identifier when known (e.g. `com.apple.Safari`) |
| `title` | string | | the frontmost window's title; empty without Screen Recording |
| `afk` | boolean | ✔ | `true` = away from keyboard (no input past the idle threshold) |

Omit nothing: writers emit every field so a line is self-describing.
Unknown fields are tolerated.

## Examples

```jsonl
{"start":"2026-06-10T14:03:01-07:00","end":"2026-06-10T14:09:22-07:00","seconds":381,"app":"Code","bundle_id":"com.microsoft.VSCode","title":"activity.rs — trove","afk":false}
{"start":"2026-06-10T14:09:22-07:00","end":"2026-06-10T14:11:02-07:00","seconds":100,"app":"Safari","bundle_id":"","title":"","afk":false}
{"start":"2026-06-10T14:11:02-07:00","end":"2026-06-10T14:26:40-07:00","seconds":938,"app":"","bundle_id":"","title":"","afk":true}
```

## Read-time semantics (FYI for writers)

Readers sum `seconds` by `app` over a date range, split active from AFK,
and render one day as a timeline in file order. Spans are expected to be
contiguous and non-overlapping within one writer's stream; a gap between
one span's `end` and the next's `start` means the sampler was not running
(machine asleep, collector stopped) and is shown as such, never
interpolated. Because the root day files are single-writer, a second
sampler must write its own `activity/<source>/` folder; readers that merge
sources do it at read time.
