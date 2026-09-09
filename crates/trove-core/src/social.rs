//! The `social` domain contract: content the vault owner **authored** on
//! social platforms — posts, comments, replies, reposts, quotes, edits — in
//! one normalized, source-agnostic event stream.
//!
//! Each source writes full-fidelity rows under `social/<source>/YYYY-MM.jsonl`
//! (`<source>` is the collector id and the folder name; the month is the month
//! of [`Post::ts`]). The stream is **append-only** — an authored item happens
//! once, like a [`crate::correspondence::Message`] — and imports skip guids
//! they already hold ([`Post::guid`] is the dedupe key). Bluesky/Mastodon poll
//! their open APIs; X/Reddit/Meta/TikTok/Tumblr/Pinterest import archive
//! exports; Hacker News and Wikipedia poll keyless public APIs; Substack
//! imports a writer export — all converge here, and a reader scans `social/*/`
//! and sees one authored-content timeline. [`Post::kind`] separates a top-level
//! post from a reply, repost, quote, or wiki edit.
//!
//! Only the user's own output lands here. Archive **DMs** route to
//! `correspondence/` (a conversation, not a post); likes, saves, bookmarks,
//! follows, and profile snapshots stay **per-source raw** under
//! `social/<source>/raw/` (they are not authored content); watch history routes
//! to `media/plays/`. Engagement counts the user *received* (likes, reposts,
//! replies, score, edit size-diff) are not authored content — they ride in
//! [`Post::extra`], never as columns. Attached media is **metadata only**,
//! never copied into the vault (the "files are the source of truth, don't
//! duplicate" rule); [`Media`] carries the in-source `url`, `type`, and the
//! author's `alt` text.
//!
//! See [`docs/vault-spec/domains/social.md`] for the field-level spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One attached-media metadata entry on a [`Post`]: `{type, url, alt}`. Never
/// a copy of the file — `url` is whatever locator the source carried (a CDN
/// URL, or an in-archive relative path for an export). Forward-compatible:
/// unknown keys an export carries are tolerated on deserialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Media {
    /// Media type: `image` | `video` | `gif` (source-native where it gives
    /// one; best-effort otherwise).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub r#type: String,
    /// Locator for the attached media — a CDN URL, or an export's in-archive
    /// relative path. Metadata only; the file is never fetched or copied.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// The author's alt text / description for the media item.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alt: String,
}

/// One authored item — one line of `social/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`guid`
/// are required; everything else is omit-empty, so a sparse source (a Wikipedia
/// edit, a bare archived post) writes a minimal line while a rich source (a
/// Bluesky reply with media, a quoted thread) fills more. The core columns are
/// what the read-time person/conversation graph and any reader key on;
/// everything source-specific they don't carry is preserved verbatim under
/// [`extra`](Post::extra) rather than dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Post {
    /// RFC3339 local time the item was authored (publish/post/edit time).
    /// Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`facebook`,
    /// `bluesky`, `hacker-news`). Always serialized.
    pub source: String,
    /// Source-unique id, the dedupe key (Bluesky record CID/rkey, Mastodon
    /// status URI, tweet id, Reddit fullname, HN item id, `{domain}:{revid}`
    /// for a wiki edit, a post slug; a hash of (section, ts, text) where an
    /// export carries no stable id). Always serialized.
    pub guid: String,
    /// `"post"` (default, a top-level item) | `"comment"` | `"reply"` |
    /// `"repost"` | `"quote"` | `"edit"` — an open string; readers stay lenient
    /// to values beyond the documented set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// The body, full fidelity (an untruncated X `full_text`, a caption, a
    /// Reddit comment, a wiki edit comment) — trimming is a read-time opinion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// The item's title where it has one: HN story, Substack post,
    /// Tumblr/Reddit title, the edited page's title for `kind:"edit"`. (A
    /// Facebook post has no user title — FB's synthesized "X posted a photo"
    /// rides in `extra.fb_title`, not here.)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The canonical link: a link-post's target, an HN story URL, or the
    /// item's own permalink.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Language tag the source recorded (Mastodon `language`, Bluesky `langs`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lang: String,
    /// Source-native id of the parent this replies/comments on (raw handle,
    /// not resolved).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reply_to: String,
    /// Source-native id of the reposted/boosted/reblogged item (`kind:"repost"`
    /// — carries no `text` of its own).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repost_of: String,
    /// Source-native id of the quoted item (`kind:"quote"` — a repost that
    /// *does* add `text`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quote_of: String,
    /// Conversation/thread root id, when the source exposes one (Bluesky reply
    /// root, Mastodon conversation, Reddit submission of a comment).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thread: String,
    /// Where it was authored: subreddit, Mastodon visibility, Tumblr/Substack
    /// blog, wiki domain — a source-native grouping label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// Author-applied tags/hashtags the source records structurally (Tumblr
    /// tags, Facebook post tags).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Attached media **metadata only**, never copies: `{type, url, alt}` per
    /// item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<Media>,
    /// Everything source-specific the normalized fields don't carry —
    /// engagement counts, post type, revision ids, sizediff, reblog flags,
    /// FB's synthesized title — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Post {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, guid: impl Into<String>, ts: impl Into<String>) -> Self {
        Post {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            kind: String::new(),
            text: String::new(),
            title: String::new(),
            url: String::new(),
            lang: String::new(),
            reply_to: String::new(),
            repost_of: String::new(),
            quote_of: String::new(),
            thread: String::new(),
            context: String::new(),
            tags: Vec::new(),
            media: Vec::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_record_serializes_only_required_fields() {
        let p = Post::new("facebook", "abc123", "2026-06-10T07:42:13-07:00");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"ts": "2026-06-10T07:42:13-07:00", "source": "facebook", "guid": "abc123"})
        );
    }

    #[test]
    fn full_record_round_trips() {
        let line = json!({
            "ts": "2026-06-10T14:03:01-07:00",
            "source": "bluesky",
            "guid": "at://did:plc:abc123/app.bsky.feed.post/3kxy",
            "kind": "reply",
            "text": "totally agree — the CAR export is the cleanest part",
            "lang": "en",
            "reply_to": "at://did:plc:xyz789/app.bsky.feed.post/3kw2",
            "thread": "at://did:plc:xyz789/app.bsky.feed.post/3kw0",
            "media": [{"type": "image", "url": "https://cdn.bsky.app/img/abc.jpg", "alt": "a screenshot of the repo viewer"}],
            "extra": {"likeCount": 12, "repostCount": 3}
        });
        let p: Post = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(p.kind, "reply");
        assert_eq!(p.reply_to, "at://did:plc:xyz789/app.bsky.feed.post/3kw2");
        assert_eq!(p.media.len(), 1);
        assert_eq!(p.media[0].r#type, "image");
        assert_eq!(p.media[0].alt, "a screenshot of the repo viewer");
        assert_eq!(serde_json::to_value(&p).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_extra_round_trips() {
        // Forward-compat: an unknown top-level field is ignored; `extra`
        // carries the source-specific bag verbatim. A media item with an extra
        // key also tolerated.
        let line = json!({
            "ts": "2026-05-22T09:41:00-07:00",
            "source": "hacker-news",
            "guid": "41250912",
            "kind": "post",
            "title": "Show HN: A local-first personal data vault",
            "future_field": "ignored",
            "media": [{"type": "image", "url": "u", "width": 800}],
            "extra": {"score": 214, "descendants": 88}
        });
        let p: Post = serde_json::from_value(line).unwrap();
        assert_eq!(p.title, "Show HN: A local-first personal data vault");
        assert_eq!(p.extra.get("score"), Some(&json!(214)));
        // Re-serialized form drops the unknown field but keeps extra and media.
        let re = serde_json::to_value(&p).unwrap();
        assert!(re.get("future_field").is_none());
        assert_eq!(re["extra"]["descendants"], json!(88));
        assert_eq!(re["media"][0]["type"], json!("image"));
    }
}
