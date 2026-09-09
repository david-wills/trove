# Domain: reading

Everything the user saved, read, or marked up on the web and in books: read-later
saves, bookmarks, RSS reads, and the highlights/annotations made on top of them.
Read-later and bookmark services (Raindrop, Instapaper, Pocket, Pinboard, Omnivore,
Wallabag, Linkding), cloud and local RSS readers (Feedly, Inoreader, NetNewsWire,
Reeder), and Readwise Reader all write the **item** shape; highlight hubs and
annotators (Readwise, Kindle clippings, Hypothesis, Snipd) write the **highlight**
shape. Readwise spans both (Reader → items, Readwise → highlights). Readers see one
saved-and-read timeline plus one annotation stream regardless of which app produced
them; cross-source overlap (the same article saved in two apps, the same Kindle
highlight via both `kindle` and `readwise`) is reconciled at read time — each source
keeps its own folder and stable guids.

- **Layout:** `reading/<source>/YYYY-MM.jsonl` (saved/read items, month of `ts`) +
  `reading/<source>/highlights/YYYY-MM.jsonl` (annotations, month of `ts`)
- **Kind:** append-only event streams
- **Schemas:**
  [`schemas/reading.item.schema.json`](../schemas/reading.item.schema.json),
  [`schemas/reading.highlight.schema.json`](../schemas/reading.highlight.schema.json)
- **Dedupe key:** `guid` (source-unique: Raindrop/Linkding/Wallabag/Reader item id,
  Readwise/Hypothesis annotation id, Pinboard hash, URL+saved-at hash for export
  files, hash(title,location,added) for Kindle clippings). Imports must skip
  already-stored guids.

A `feed-subscription` list, OPML import, page snapshot, or article full-text body is
**not** an item or a highlight — those stay per-source raw under
`reading/<source>/` (e.g. `feeds.jsonl`, `raw/`). Only the saved/read entries and the
annotations join the contract. Saved *social* posts never route here (they stay under
`social/<source>/`); a book read as a play belongs in `media/plays/`.

## The item — `reading/<source>/YYYY-MM.jsonl`

One saved/read article, bookmark, or RSS item per line. Only `ts`/`source`/`guid`
are required — a bare bookmark writes three fields; a richly-tagged read-later save
with progress fills more. `ts` is the most identity-bearing time the source exposes:
the save time for bookmarks, the read/published time for RSS items, the import date
for an undated export row (original position preserved in `extra`).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the item was saved (or read/published, per source) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id, the dedupe key |
| `url` | string | | the saved/linked page |
| `title` | string | | article / bookmark title |
| `author` | string | | byline, where the source carries one |
| `site` | string | | publisher / domain (`"example.com"`) |
| `feed` | string | | RSS feed / publication title, for reader sources |
| `excerpt` | string | | snippet, selection, or the bookmark's own note/description |
| `tags` | string[] | | user tags / folders / collection labels |
| `state` | string | | `"saved"` (default, unread) \| `"archived"` \| `"read"` \| `"favorite"` (starred) |
| `progress` | int | | read progress as an integer percent, 0–100 (Instapaper, Reader) |
| `read_at` | string | | RFC3339 local time the item was read / last opened |
| `extra` | object | | everything source-specific (collection name, cover, shared flag, original CSV position, …) |

## The highlight — `reading/<source>/highlights/YYYY-MM.jsonl`

One highlighted passage or annotation per line, with its **parent document referenced
inline** so no join is needed: a book carries `title` + `author`, a web page carries
`url` + `title`. Only `ts`/`source`/`guid` are required — `text` is omitted on rows
that have none (a Kindle bookmark, or a clipping that hit Amazon's clipping-limit cap,
which records its marker in `extra` instead). A passage with no user annotation simply
omits `note`.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the highlight was made / added |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id, the dedupe key |
| `text` | string | | the highlighted passage (omitted for bookmarks / capped clippings) |
| `note` | string | | the user's annotation / note on the passage |
| `title` | string | | parent document title (book or page) |
| `author` | string | | parent author, for books |
| `url` | string | | parent page URL, for web annotations |
| `location` | string | | page / Kindle location range / position / CFI / in-episode timestamp |
| `color` | string | | highlight color, where the source has one |
| `tags` | string[] | | tags on the highlight |
| `extra` | object | | everything source-specific (AI summary, group id, category, `highlighted_at`, clipping-limit flag, …) |

## Examples

```jsonl
{"ts":"2026-06-10T14:03:00-07:00","source":"raindrop","guid":"rd-1029384","url":"https://example.com/a-deep-dive","title":"A Deep Dive into Local-First Software","site":"example.com","excerpt":"The cloud is just someone else's computer.","tags":["software","local-first"],"state":"saved","progress":63,"extra":{"collection":"Reading","cover":"https://example.com/cover.jpg"}}
{"ts":"2026-06-09T07:41:00-07:00","source":"inoreader","guid":"tag:google.com,2005:reader/item/000000023ab1","url":"https://blog.example.org/rss-is-not-dead","title":"RSS Is Not Dead","author":"Jane Roe","site":"blog.example.org","feed":"Example Engineering Blog","state":"read","read_at":"2026-06-09T08:02:00-07:00","extra":{"folder":"Tech"}}
{"ts":"2024-02-18T22:15:00-08:00","source":"pinboard","guid":"a1b2c3d4e5f6","url":"https://news.example.net/old-but-gold","title":"Old But Gold","tags":["archive","reference"]}
```

```jsonl-highlight
{"ts":"2026-06-08T20:11:00-07:00","source":"readwise","guid":"rw-hl-884412","text":"Attention is the rarest and purest form of generosity.","note":"cf. Weil on prayer","title":"Gravity and Grace","author":"Simone Weil","location":"142","color":"yellow","tags":["attention","ethics"],"extra":{"category":"books","highlighted_at":"2026-06-08T20:11:00-07:00"}}
{"ts":"2026-06-07T11:30:00-07:00","source":"hypothesis","guid":"AaBbCc-annot-01","text":"The map is not the territory.","note":"Useful framing for the data-model section.","url":"https://example.com/korzybski-essay","title":"On General Semantics","tags":["epistemics"],"extra":{"group":"__world__"}}
{"ts":"2026-05-30T09:05:00-07:00","source":"kindle","guid":"f3a9c1b27e","text":"We are what we repeatedly do.","title":"The Nicomachean Ethics","author":"Aristotle","location":"1099-1101"}
```

## Read-time semantics (FYI for writers)

The reading reader scans `reading/*/*.jsonl` for the saved-and-read timeline and
`reading/*/highlights/*.jsonl` for annotations; creating those folders is the
registration. Items and highlights from the same provider live side by side and join
on `url`/`title` at read time — the contract never requires the highlight to point at
a stored item, because the highlighting source (Readwise, Kindle) is often not the
saving source. `state` is a coarse normalization of each app's own vocabulary
(Instapaper folders, Pinboard `toread`, Wallabag/Linkding archived/starred, RSS
read/starred); the source's exact flags stay in `extra`. Cross-source dedupe (one
article saved in two apps; a highlight arriving via both `kindle` and `readwise`) is a
read-time opinion — write your own folder with stable guids and let the reader
reconcile. Full article text, page snapshots, and feed lists are deliberately *not*
contract fields; they live in each source's raw folder at full fidelity.
