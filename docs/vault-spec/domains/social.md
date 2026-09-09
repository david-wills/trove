# Domain: social

The content the vault owner **authored** on social platforms — posts,
comments, replies, reposts, quotes — as one stream. Bluesky and Mastodon
poll their open APIs; X/Reddit/Meta/TikTok/Tumblr/Pinterest archive exports
import; Hacker News and Wikipedia poll keyless public APIs; Substack imports
a writer export. All write this one shape and readers see one authored-content
timeline regardless of platform. Only the user's own output lands here: archive
**DMs** route to `correspondence/` (a conversation, not a post); likes, saves,
bookmarks, follows, and profile snapshots stay **per-source raw** under
`social/<source>/` (they are not authored content, and saved posts never route
to `reading/`); watch history routes to `media/plays/`.

- **Layout:** `social/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/social.post.schema.json`](../schemas/social.post.schema.json)
- **Dedupe key:** `guid` (source-unique: Bluesky record CID/rkey, Mastodon
  status URI, tweet id, Reddit fullname, HN item id, `{domain}:{revid}` for
  a wiki edit, post slug; a hash of (section, ts, text) where an export
  carries no stable id). Imports must skip already-stored guids.

## Post

One record per authored item. Only `ts`/`source`/`guid` are required — a
sparse source (a Wikipedia edit, a bare archived post) writes a minimal line;
a rich source (a Bluesky reply with media, a quoted thread) fills more.
Engagement counts the user *received* (likes, reposts, replies, score, edit
size-diff) are not authored content — they ride in `extra`, not as columns.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the item was authored (publish/post/edit time); a date-only `YYYY-MM-DD` is accepted where the source is day-granular (ratified 2026-07-20 — dates sort as lexical prefixes of timestamps) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id, the dedupe key |
| `kind` | string | | `"post"` (default, a top-level item) \| `"comment"` \| `"reply"` \| `"repost"` \| `"quote"` \| `"edit"` — Reddit submissions + comments, HN stories + comments, microblog replies/boosts/quote-posts, and wiki edits all converge here |
| `text` | string | | the body, full fidelity (X `full_text` untruncated, a caption, a Reddit comment, a wiki edit comment) — trimming is a read-time opinion |
| `title` | string | | the item's title where it has one: HN story, Substack post, Tumblr/Reddit title, the edited page's title for `kind:"edit"` |
| `url` | string | | the canonical link: a link-post's target, an HN story URL, or the item's own permalink |
| `lang` | string | | language tag the source recorded (Mastodon `language`, Bluesky `langs`) |
| `reply_to` | string | | source-native id of the parent this replies/comments on (raw handle, not resolved) |
| `repost_of` | string | | source-native id of the reposted/boosted/reblogged item (`kind:"repost"` — carries no `text` of its own) |
| `quote_of` | string | | source-native id of the quoted item (`kind:"quote"` — a repost that *does* add `text`) |
| `thread` | string | | conversation/thread root id, when the source exposes one (Bluesky reply root, Mastodon conversation, Reddit submission of a comment) |
| `context` | string | | where it was authored: subreddit, Mastodon visibility, Tumblr/Substack blog, wiki domain — a source-native grouping label |
| `tags` | string[] | | author-applied tags/hashtags the source records structurally (Tumblr tags) |
| `media` | object[] | | attached media **metadata only**, never copies: `{type, url, alt}` per item (image/video/gif; `alt` = the author's alt text / description) |
| `extra` | object | | everything source-specific (engagement counts, post type, revision ids, sizediff, reblog flags, …) — full fidelity |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T14:03:01-07:00","source":"bluesky","guid":"at://did:plc:abc123/app.bsky.feed.post/3kxy","kind":"reply","text":"totally agree — the CAR export is the cleanest part","lang":"en","reply_to":"at://did:plc:xyz789/app.bsky.feed.post/3kw2","thread":"at://did:plc:xyz789/app.bsky.feed.post/3kw0","media":[{"type":"image","url":"https://cdn.bsky.app/img/abc.jpg","alt":"a screenshot of the repo viewer"}],"extra":{"likeCount":12,"repostCount":3}}
{"ts":"2026-05-22T09:41:00-07:00","source":"hacker-news","guid":"41250912","kind":"post","title":"Show HN: A local-first personal data vault","text":"Built this over the last year to keep my own data on my own machine.","url":"https://example.com/trove","extra":{"score":214,"descendants":88}}
{"ts":"2026-06-12T18:20:33+00:00","source":"wikipedia","guid":"en.wikipedia.org:1259884412","kind":"edit","title":"AT Protocol","text":"/* History */ add 2024 OAuth milestone, fix ref","extra":{"sizediff":312,"revid":1259884412,"parentid":1259880001}}
```

## Read-time semantics (FYI for writers)

The social reader scans `social/*/` for an authored-content timeline; creating
your source folder is the registration. `kind` separates a fresh post from a
reply or repost so views can show "what I wrote" without counting boosts as
original writing. `reply_to`/`quote_of`/`thread` are **raw source ids**, joined
into conversation trees at read time — write the id the platform gave you, never
a resolved person or a guessed parent. The same row is the only normalized copy,
but the source's own `social/<source>/raw/` keeps full fidelity, so a missing
column is never data loss. Wikipedia edits sit in this stream as `kind:"edit"`
(page title in `title`, edit comment in `text`); a reader that wants only
microblog posts filters them out by `kind` — they are catalogued here as public
authored contributions, the closest shape in the taxonomy, and flagged for
David's review (see the manifest's open questions).
