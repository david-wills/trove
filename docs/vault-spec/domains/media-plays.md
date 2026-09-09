# Domain: media-plays

The **write contract** of the unified media stream: listens, watches, and
plays from any service — Letterboxd, Trakt, Spotify exports, a vinyl-log
script — land here and merge (at read time) with Trove's built-in arms
(Apple Music scrobbles, podcasts, audible web spans, iPhone Now Playing).

- **Layout:** `media/plays/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/media.play.schema.json`](../schemas/media.play.schema.json)
- **Dedupe key:** `guid` (source-unique) for re-runnable imports.

Source folders are discovered by scanning — no registration, no code
change; rows appear in the Media tab and the generic data browser.

## Fields

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the play happened (started, for spans) |
| `source` | string | ✔ | collector id, = the folder name (may be `""` in sparse lines; the folder then names it) |
| `category` | string | ✔ | content type: `"music"` \| `"podcast"` \| `"audiobook"` \| `"video"` \| `"other"` — the UI's filter axis |
| `kind` | string | ✔ | `"play"` (a real listen/watch) \| `"partial"` (skip, unfinished) |
| `title` | string | ✔ | track / episode / film title |
| `subtitle` | string | ✔ | artist / show / director / site — the grouping key for top charts |
| `seconds` | int | ✔ | seconds actually played; `0` when unknown |
| `detail` | string | | album / URL / chapter |
| `device` | string | | device label the play came from, when known |
| `favicon` | string | | icon URL, if any |
| `guid` | string | | source-unique id, the dedupe key |
| `extra` | object | | everything source-specific (ratings, rewatch flags, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T21:00:00-07:00","source":"letterboxd","category":"video","kind":"play","title":"Heat","subtitle":"Michael Mann","detail":"https://letterboxd.com/film/heat-1995/","seconds":10260,"guid":"lb-heat-1995","extra":{"rating":"5","rewatch":"true"}}
{"ts":"2026-06-11T08:15:00-07:00","source":"spotify-export","category":"music","kind":"partial","title":"Halah","subtitle":"Mazzy Star","seconds":45}
```

## Read-time semantics (FYI for writers)

Top charts group by `subtitle` within a `category`; `kind:"play"` counts as
a play, `"partial"` contributes only seconds. If your source can't measure
seconds, write `0` — honest unknowns beat invented numbers. Don't write
rows that duplicate a built-in arm (e.g. re-importing Apple Music plays a
scrobbler already captured) unless your `guid`s make re-runs safe.
