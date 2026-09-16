# Domain: browser-visits

Every web page the owner visited, with how long they stayed, from every
browser and from two kinds of source that deliberately overlap: a **history
import** (copy-then-read of the browser's own history database — complete
but shallow, no durations for Safari, subject to the browser's retention
window) and a **live extension** (a browser extension reporting tab
engagement in real time — rich, but only while it runs). Both write the
same stream; `source` tells them apart, and readers resolve the overlap in
the extension's favour. Searches are a sibling stream
([browser-searches](browser-searches.md)); ads seen on these pages are
another ([ads](ads.md)).

- **Layout:** `browser/YYYY-MM-DD.jsonl` (day of `time`) — **multi-writer**:
  the history sync and one extension host per browser profile all append
  to the same day file, so every writer takes an exclusive `flock` on the
  partition file around its append (see [conventions](../conventions.md)).
- **Kind:** append-only event stream
- **Schema:** [`schemas/browser.visit.schema.json`](../schemas/browser.visit.schema.json)
- **Dedupe key:** none stored. The history import keeps a cursor per
  browser/profile (rebuildable from the stream) so it never re-imports a
  visit; the extension writes each span once, when it closes. The *same
  browsing* observed by both sources is expected and resolved at read time.

## Visit

One visit per line. `time`, `url`, and `browser` are required; `source`
defaults to `"history"` when absent. History rows carry `profile` and,
where the browser records it, `duration_secs`. Extension rows leave
`profile` empty (the tabs API does not expose it) and add the engagement
model: `duration_secs` is the engaged wall time — the URL was the active
tab of the focused window *or* was audible in any tab — and
`foreground_secs` is the part of that spent as the focused-active tab, so a
reader can attribute media consumption without billing background
playback into focus time. `audible` marks spans that played audio at any
point. The remaining extension-only fields are cheap context: favicon,
referrer, navigation transition, and open-tab count.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `time` | string | ✔ | RFC3339 local time the visit (or span) began |
| `url` | string | ✔ | the page URL |
| `title` | string | | the page title, when known |
| `browser` | string | ✔ | `"chrome"`, `"safari"`, … (lowercase) |
| `profile` | string | | browser profile name (history rows); empty for extension rows |
| `duration_secs` | integer | | time on the page — the browser's own figure for history rows, the engaged wall time for extension rows; 0/absent when unknown |
| `source` | string | | provenance: `"history"` (default) or `"extension"` |
| `audible` | boolean | | extension rows: the tab played audio at some point in the span |
| `foreground_secs` | integer | | extension rows: seconds as the focused-active tab (≤ `duration_secs`) |
| `favicon` | string | | extension rows: the tab's favicon URL |
| `referrer` | string | | extension rows: the page this visit was navigated from, when the browser could tell |
| `transition` | string | | extension rows: how the navigation happened (`link`, `typed`, `reload`, `form_submit`, …) |
| `tab_count` | integer | | extension rows: open tabs across all windows when the span opened |

Omit empty/zero/false optional fields. Unknown fields are tolerated.

## Examples

```jsonl
{"time":"2026-06-10T14:03:01-07:00","url":"https://news.ycombinator.com/","title":"Hacker News","browser":"chrome","profile":"Default","duration_secs":42,"source":"history"}
{"time":"2026-06-10T14:03:05-07:00","url":"https://www.youtube.com/watch?v=x","title":"Some video","browser":"chrome","profile":"","duration_secs":1840,"source":"extension","audible":true,"foreground_secs":95,"favicon":"https://www.youtube.com/favicon.ico","referrer":"https://www.google.com/","transition":"link","tab_count":12}
{"time":"2026-06-11T09:15:44-07:00","url":"https://example.com/","browser":"safari","profile":"","source":"history"}
```

## Read-time semantics (FYI for writers)

Readers scan a day file, then drop each `"history"` row whose URL and time
fall inside an `"extension"` span for the same URL (with a couple of
minutes' slack), so the same browsing is counted once, from the richer
observation. History rows with no covering span pass through untouched:
that is the backup contract — visits the extension missed (it was off, the
browser had no extension, the machine was asleep) still count. The raw
file keeps every row from every writer; the precedence is a read, not a
rewrite.
