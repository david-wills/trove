# Domain: browser-searches

Every search query the owner has typed, from any search source, in one
stream. Google's My Activity Search log (via Takeout) and the search-engine
visits in Safari's history both reduce to the same atom — **one query at one
time** — and write one record each; readers list them chronologically and
group by `engine` for "what I searched on Google vs. DuckDuckGo". This is the
query stream only: the page the owner *visited* after searching is a
`browser/` visit, a sibling stream, joined (if at all) at read time. Searches
carry intent and are read-sensitive; sources ship opt-in alongside the
browser-history grant they already require.

- **Layout:** `browser/searches/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/browser-searches.search.schema.json`](../schemas/browser-searches.search.schema.json)
- **Dedupe key:** `guid` (source-unique: a `(time, query)` hash for Takeout
  My Activity rows, which carry no native id; a search-URL-visit hash for
  Safari). Overlapping re-exports must skip already-stored guids before
  appending.

## Search

One search query per line. Only `ts`, `source`, and `query` are required —
that triple is the whole record a sparse source needs; `engine`, `url`, and
the dedupe `guid` are optional enrichment a richer source fills. The
collector owns extracting a clean `query` string (Takeout's `title` is a
localized `"Searched for …"` sentence and its `titleUrl` is a
`?q=`-encoded search URL; Safari's is the `q=`/`p=`/`query=` parameter of a
visited search-engine URL) — the contract receives the decoded terms, never
the wrapper.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the search was issued |
| `source` | string | ✔ | collector id, = the folder name |
| `query` | string | ✔ | the search terms, decoded (URL-unescaped, prefix stripped) |
| `engine` | string | | the search engine, lowercased: `"google"`, `"bing"`, `"duckduckgo"`, `"safari-default"` (Safari's address-bar default when the host is opaque), … — the read-time grouping axis |
| `url` | string | | the full search URL, when the source has one (Takeout `titleUrl`; the visited Safari URL) |
| `guid` | string | | source-unique id, the dedupe key |
| `extra` | object | | everything source-specific (Takeout `header`/`products`/`locationInfos`, Safari visit count, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T21:03:01-07:00","source":"google-takeout","query":"trove app local first","engine":"google","url":"https://www.google.com/search?q=trove+app+local+first","guid":"gt-search-2026-06-10T21:03:01-trove+app+local+first","extra":{"header":"Search","products":["Search"]}}
{"ts":"2026-06-11T08:42:17-07:00","source":"google-takeout","query":"how to defrost sourdough"}
{"ts":"2026-06-11T09:15:44-07:00","source":"safari","query":"flights to lisbon","engine":"duckduckgo","url":"https://duckduckgo.com/?q=flights+to+lisbon","guid":"sf-search-3f9a1c0b"}
```

## Read-time semantics (FYI for writers)

The searches reader scans `browser/searches/*/`; creating your source folder
is the registration. Group by `engine` within a window for query histograms,
or by day for a "what was I looking into" timeline; `query` text is the
search index. **What I clicked is not here** — neither queued source can
observe which result the owner opened (Takeout logs the search event, not the
follow-on click; Safari history is a flat visit list with no search→result
link), so the contract carries no `result_clicked` field. A future
SERP-aware source could add it additively; until one exists, the click is a
read-time *guess* (the next `browser/` visit after a search), never a written
fact. Write what you observed; never persist a derived join back into the
vault.
