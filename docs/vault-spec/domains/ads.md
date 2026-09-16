# Domain: ads

The display ads the owner was shown while browsing, one record per ad
impression: where it appeared, who served it, who (as far as can be told)
paid for it, how big it was, and how long it was actually on screen. Pure
observation — the source never blocks ads, never captures the creative or
the page text, and stores only URLs and numbers. It is written by a
browser extension's opt-in page observer (Chrome requires a site-access
grant for it) through the same always-on collector that writes
[browser-visits](browser-visits.md); the pages the ads appeared on are in
that stream, joined by `page_url` and time at read time if at all.

- **Layout:** `browser/ads/YYYY-MM-DD.jsonl` (day of `end`, the *close*
  time — an ad is recorded when it leaves the page, so its viewing time is
  final). Multi-writer like its parent: one extension host per browser
  profile appends here, each taking an exclusive `flock` around its append.
- **Kind:** append-only event stream
- **Schema:** [`schemas/browser.ad.schema.json`](../schemas/browser.ad.schema.json)
- **Dedupe key:** none — each closed impression is written once.

## Ad

One impression per line. `ts`, `end`, `page_url`, and `network` are
required; `source` defaults to `"extension"`. The extension is a dumb
sensor shipping raw URLs; the collector derives `network` (registrable
domain of the ad frame's URL, or the ad-slot family such as `"google"` for
frames with no URL, or `"unknown"`) and `advertiser` (registrable domain of
the click-through landing URL, when the extension could join one) before
writing. With the opt-in `browser-ads-identify` toggle on, the collector
may instead fill `advertiser` with the legal "Paid for by" name read off
Google's public ad-transparency page for that creative, plus its
`advertiser_id`; that lookup is the only networked step anywhere in the
ads path and is off by default. Viewability follows the MRC display
standard the events were measured with: `viewable` is true when the ad was
at least half visible for at least one continuous second, and
`viewed_secs` is the total time it was at least half visible in a visible
tab.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the ad was detected |
| `end` | string | ✔ | RFC3339 local time it closed (removed / page unloaded); the file is keyed by this day |
| `page_url` | string | ✔ | the top-frame page the ad appeared on |
| `frame_url` | string | | the ad iframe's URL; empty for srcdoc / about:blank frames |
| `landing_url` | string | | click-through landing URL, when joined; empty otherwise |
| `network` | string | ✔ | serving network: registrable domain of `frame_url`, else the slot family (`"google"`), else `"unknown"` |
| `advertiser` | string | | registrable domain of `landing_url`, or the resolved "Paid for by" name; empty when unattributable |
| `advertiser_id` | string | | Google ad-transparency advertiser id (`AR…`) when the resolver named `advertiser` |
| `viewed_secs` | number | | seconds ≥50 % visible in a visible tab |
| `viewable` | boolean | | met the MRC bar (≥50 % visible for ≥1 s) |
| `w` | integer | | largest observed frame width, CSS px |
| `h` | integer | | largest observed frame height, CSS px |
| `source` | string | | `"extension"` (default) |

Omit empty/zero/false optional fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-11T10:23:45-07:00","end":"2026-06-11T10:24:10-07:00","page_url":"https://example.com/article","frame_url":"https://googleads.g.doubleclick.net/pagead/ads?x","landing_url":"https://www.advertiser.com/promo","network":"doubleclick.net","advertiser":"advertiser.com","viewed_secs":12.5,"viewable":true,"w":300,"h":250,"source":"extension"}
{"ts":"2026-06-11T10:30:00-07:00","end":"2026-06-11T10:30:04-07:00","page_url":"https://example.com/article","network":"google","w":728,"h":90,"source":"extension"}
{"ts":"2026-06-11T11:02:12-07:00","end":"2026-06-11T11:05:40-07:00","page_url":"https://news.example.org/story","frame_url":"https://tpc.googlesyndication.com/sf","network":"googlesyndication.com","advertiser":"Hearts & Science LLC","advertiser_id":"AR04055566952792326145","viewed_secs":31.0,"viewable":true,"w":300,"h":600}
```

## Read-time semantics (FYI for writers)

Readers list a day's impressions in file order and aggregate over a range
by `network` and by `advertiser` (count, viewable count, total
`viewed_secs`), ranked by viewed time. Records with an empty `advertiser`
count toward the network but not the advertiser table — an unattributable
ad is reported as such rather than guessed. Nothing here is joined back to
the page visit at write time; a reader that wants "ads per site" groups by
the registrable domain of `page_url` itself.
