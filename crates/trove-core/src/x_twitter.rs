//! X (Twitter) — official archive import for posts and DMs.
//!
//! The only viable path for general users is the **official account archive
//! ZIP** (Settings → Your Account → Download an archive of your data). No
//! auth, no network, no TCC — standalone-clean. API v2 is pay-per-use and
//! deliberately skipped.
//!
//! ## What lands where
//!
//! - **Tweets** → BOTH layers: the normalized `social` contract row
//!   (`social/x-twitter/YYYY-MM.jsonl`, [`crate::social::Post`]) is the
//!   curated view; the full decoded tweet object is ALSO written to the raw
//!   layer (`social/x-twitter/raw/tweets.jsonl`). `kind` distinguishes
//!   top-level posts ("post"), replies ("reply"), and retweets ("repost").
//! - **DMs** (1:1 and group) → the ratified correspondence contract
//!   (`correspondence/x-twitter/YYYY-MM.jsonl`,
//!   [`crate::correspondence::Message`]). `from_me` is derived from the
//!   archive owner's `accountId` found in `account.js`; without it every
//!   message is recorded received-only (still useful for thread reading).
//! - **Everything else** (likes, follower/following lists, ad engagements,
//!   etc.) → `social/x-twitter/raw/<section>.jsonl`, full-fidelity.
//!
//! ## Archive format
//!
//! Twitter/X produces a ZIP with a `data/` directory of per-section `.js`
//! files. Each file is a **JavaScript module** of the form:
//!
//! ```js
//! window.YTD.tweets.part0 = [
//!   { "tweet": { … } },
//!   …
//! ]
//! ```
//!
//! Stripping: discard the first line (the assignment preamble), replace it
//! with `[`, concatenate the rest, and parse as JSON. Handles multi-part
//! files (e.g. `tweet.js`, `tweets-part1.js`) by treating each file as an
//! independent slice.
//!
//! ## Deduplication
//!
//! `tweet_id` / DM `messageCreate.id` are stable source ids — the `guid`
//! dedupe key. Re-importing a newer or identical archive dedupes cleanly.
//!
//! ## Privacy
//!
//! DM bodies are intimate. An explicit opt-in acknowledgement is required
//! before the archive is read (presented as a required import param).

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportParam, ImportSpec, IntegrationDef};
use crate::social::{Media, Post};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "x-twitter";
const SOCIAL_DIR: &str = "social/x-twitter";
const RAW_DIR: &str = "social/x-twitter/raw";
const CORR_SOURCE: &str = "x-twitter";
// The raw-section name for the decoded tweet objects kept full-fidelity
// alongside the normalized Post contract rows.
const TWEETS_SECTION: &str = "tweets";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(SOCIAL_DIR))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join("correspondence/x-twitter")))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "x-twitter",
        name: "X (Twitter)",
        kind: IntegrationKind::Import,
        // 🔒 Archive contains DM bodies — opt-in only.
        default_on: false,
        description: "Import your X (Twitter) history from the official account archive ZIP: \
                      your tweets join the social stream and your DMs land in correspondence. \
                      Likes, follower lists, and ad data are kept full-fidelity. Re-runnable; \
                      newer archives never duplicate.",
        domain: "social",
        vault_path: "social/x-twitter/",
        toggleable: false,
        setup: &[
            "X (Twitter) → Settings → Your Account → Download an archive of your data. \
             You will receive a notification/email with a download link (usually within hours). \
             Download links expire after 7 days — import promptly.",
            "Drop the archive ZIP here as-is. The archive contains DM message bodies; \
             import only if you intend to store that data in your private vault. \
             Media files are never copied — only their metadata.",
        ],
        caveats: "Archive download links expire after 7 days — re-request the archive \
                  periodically to keep your vault current. DM media attachments and tweet \
                  media are stored as metadata only (URLs), never downloaded. Retweets \
                  are stored as kind:\"repost\" with the retweeted tweet id in repost_of.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[
        ImportParam {
            // 🔒 Opt-in acknowledgement — the hub renders this as a required
            // field so the user actively confirms before DM bodies are read.
            key: "acknowledge",
            label: "Privacy acknowledgement",
            placeholder: "Type 'yes' to confirm you want this archive (including DMs) stored in your vault",
            required: true,
        },
    ],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import runner.

#[derive(Default)]
struct Stats {
    tweets: u64,
    dms: u64,
    raw: u64,
    duplicates: u64,
    sections: HashSet<String>,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // ── Load existing guids for dedupe ──────────────────────────────────────
    let post_stream = vault.stream(SOCIAL_DIR, Partition::Month);
    let mut seen_posts: HashSet<String> = HashSet::new();
    for key in post_stream.partitions()? {
        for p in post_stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen_posts.insert(p.guid);
            }
        }
    }
    let mut seen_raw: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut seen_dms = vault.correspondence_guids(CORR_SOURCE)?;

    // ── Open the archive ZIP ────────────────────────────────────────────────
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a Twitter/X archive ZIP?", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    // ── Detect the archive owner's accountId (from account.js / account-suspension.js)
    // to resolve `from_me` for DMs. Optional — absent → from_me=false.
    let my_id = detect_owner_id(&mut zip, &names);

    let mut stats = Stats::default();
    let mut posts: Vec<Post> = Vec::new();
    let mut raw_posts: Vec<Value> = Vec::new(); // full-fidelity raw tweet objects
    let mut raw_by_section: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut messages: Vec<Message> = Vec::new();

    for name in &names {
        let lower = name.to_ascii_lowercase();
        // Only process .js files inside data/
        if !lower.ends_with(".js") || !lower.contains("data/") {
            continue;
        }

        let mut body = String::new();
        if read_entry(&mut zip, name, &mut body).is_err() {
            continue;
        }
        let stripped = match strip_js_wrapper(&body) {
            Some(s) => s,
            None => continue, // empty or header-only file
        };
        let value: Value = match serde_json::from_str(&stripped) {
            Ok(v) => v,
            Err(_) => continue, // not parseable JSON after stripping
        };

        let section = section_name(name);

        if is_tweets_file(name) {
            // ── Tweets → social contract + raw tweets ───────────────────────
            let raw_seen = seen_raw
                .entry(TWEETS_SECTION.to_string())
                .or_insert_with(|| load_raw_section_guids(vault, TWEETS_SECTION));
            for item in array_items(&value) {
                // Archive items are wrapped: {"tweet": {...}}
                let tweet_obj = item.get("tweet").unwrap_or(item);
                let Some(tweet_id) = tweet_obj.get("id_str").and_then(Value::as_str) else {
                    continue;
                };
                // Raw: full-fidelity copy (deduped by tweet id)
                let raw_guid = tweet_id.to_string();
                if raw_seen.insert(raw_guid.clone()) {
                    raw_posts.push(json!({
                        "section": TWEETS_SECTION,
                        "guid": raw_guid,
                        "raw": item.clone()
                    }));
                    stats.raw += 1;
                    stats.sections.insert(TWEETS_SECTION.to_string());
                }
                // Contract: normalized Post row
                if !seen_posts.insert(tweet_id.to_string()) {
                    stats.duplicates += 1;
                    continue;
                }
                if let Some(post) = tweet_to_post(tweet_obj) {
                    posts.push(post);
                    stats.tweets += 1;
                }
            }
        } else if is_dm_file(name) {
            // ── DMs → correspondence contract ────────────────────────────────
            // Twitter/X archive dmConversation.messages[] carries multiple entry
            // types: messageCreate (the most common), reactionCreate, joinConversation,
            // participantsJoin, participantsLeave, conversationNameUpdate, and
            // welcomeMessageCreate.  We map each to an appropriate correspondence
            // kind rather than silently dropping non-messageCreate entries.
            for item in array_items(&value) {
                let conv = item.get("dmConversation").unwrap_or(item);
                let conv_id = conv
                    .get("conversationId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let msgs = conv
                    .get("messages")
                    .and_then(Value::as_array)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                for msg_item in msgs {
                    // Identify entry type and unwrap the inner payload.
                    let (entry_type, mc) = if let Some(mc) = msg_item.get("messageCreate") {
                        ("messageCreate", mc)
                    } else if let Some(rc) = msg_item.get("reactionCreate") {
                        ("reactionCreate", rc)
                    } else if let Some(jc) = msg_item.get("joinConversation") {
                        ("joinConversation", jc)
                    } else if let Some(pj) = msg_item.get("participantsJoin") {
                        ("participantsJoin", pj)
                    } else if let Some(pl) = msg_item.get("participantsLeave") {
                        ("participantsLeave", pl)
                    } else if let Some(cu) = msg_item.get("conversationNameUpdate") {
                        ("conversationNameUpdate", cu)
                    } else if let Some(wm) = msg_item.get("welcomeMessageCreate") {
                        ("welcomeMessageCreate", wm)
                    } else {
                        // Unknown entry type: use first value found, or the item itself.
                        let inner = msg_item
                            .as_object()
                            .and_then(|o| o.values().next())
                            .unwrap_or(msg_item);
                        ("unknown", inner)
                    };

                    // Derive a stable id for deduplication. messageCreate uses "id";
                    // other types use a content hash of the entry payload.
                    let msg_id: String = if entry_type == "messageCreate" {
                        mc.get("id")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_default()
                    } else {
                        // For non-message entries without a stable id, use a hash of
                        // the full entry so re-imports dedupe correctly.
                        content_hash(msg_item)
                    };
                    if msg_id.is_empty() {
                        continue;
                    }
                    if !seen_dms.insert(msg_id.clone()) {
                        stats.duplicates += 1;
                        continue;
                    }

                    let mapped = match entry_type {
                        "messageCreate" => {
                            dm_to_message(mc, &conv_id, my_id.as_deref(), &msg_id)
                        }
                        "reactionCreate" => {
                            dm_reaction_to_message(mc, &conv_id, my_id.as_deref(), &msg_id)
                        }
                        _ => {
                            // joinConversation / participantsJoin / participantsLeave /
                            // conversationNameUpdate / welcomeMessageCreate → store as
                            // kind:"event" so nothing is lost from the DM thread.
                            dm_event_to_message(mc, entry_type, &conv_id, &msg_id)
                        }
                    };
                    if let Some(m) = mapped {
                        messages.push(m);
                        stats.dms += 1;
                    }
                }
            }
        } else if section == "account" || section == "account-suspension" {
            // Parsed above for owner id — skip raw routing (would be noisy).
            continue;
        } else {
            // ── Any other section → raw layer, full-fidelity ─────────────────
            let seen = seen_raw
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, &section));
            let bucket = raw_by_section.entry(section.clone()).or_default();
            for item in array_items(&value) {
                let guid = content_hash(&item);
                if !seen.insert(guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                bucket.push(json!({"section": &section, "guid": guid, "raw": item}));
                stats.raw += 1;
                stats.sections.insert(section.clone());
            }
        }
    }

    // ── Persist ──────────────────────────────────────────────────────────────
    // Tweets → social contract stream (partitioned by month of ts).
    post_stream.append(&posts, |p| &p.ts)?;

    // Raw tweets → social/x-twitter/raw/tweets.jsonl
    if !raw_posts.is_empty() {
        append_raw_section(vault, TWEETS_SECTION, &raw_posts)?;
    }

    // Other raw sections.
    for (section, rows) in &raw_by_section {
        if rows.is_empty() {
            continue;
        }
        append_raw_section(vault, section, rows)?;
    }

    // DMs → correspondence stream.
    vault.append_messages(&messages)?;

    progress(ImportProgress { records: stats.tweets + stats.dms + stats.raw, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{} tweets, {} DMs, {} raw items across {} sections, {} duplicates skipped",
            stats.tweets,
            stats.dms,
            stats.raw,
            stats.sections.len(),
            stats.duplicates,
        ),
        counts: [
            ("tweets", stats.tweets),
            ("dms", stats.dms),
            ("raw", stats.raw),
            ("sections", stats.sections.len() as u64),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// JS-wrapper stripping.

/// Strip the Twitter/X archive JS module wrapper and return the JSON array
/// body, or `None` when the file is empty or has no second line.
///
/// Input:  `window.YTD.tweets.part0 = [\n  { "tweet": … },\n]\n`
/// Output: `[\n  { "tweet": … },\n]\n`
///
/// The first line is always the assignment; the rest is valid JSON (the
/// leading `[` is the start of the array literal). We discard line 0 and
/// prepend `[` so multi-line content reassembles correctly. When the first
/// line *also* opens a `{` (a few older variants omit the line break), we
/// emit `[ {` instead of just `[`.
fn strip_js_wrapper(body: &str) -> Option<String> {
    let mut lines = body.splitn(2, '\n');
    let first = lines.next()?;
    let rest = lines.next().unwrap_or("");
    if rest.trim_start().is_empty() {
        return None;
    }
    // If the first line already contains a `{` (the opening of the first
    // object), the rest starts at the second key — prefix with `[ {`.
    let prefix = if first.contains('{') { "[ {" } else { "[" };
    Some(format!("{prefix}{rest}"))
}

// ---------------------------------------------------------------------------
// File routing helpers.

/// Is this a tweets file (`tweet.js` or `tweets*.js`)? Handles the Twitter
/// archive naming: `data/tweet.js`, `data/tweets.js`, and numbered parts
/// `data/tweets-part1.js`.
fn is_tweets_file(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    (file == "tweet.js" || file.starts_with("tweets") || file.starts_with("tweet-part"))
        && file.ends_with(".js")
}

/// Is this a DM file (1:1 or group)?
fn is_dm_file(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    (file.starts_with("direct-messages") || file.starts_with("direct-message-group"))
        && file.ends_with(".js")
}

/// A stable, human-meaningful section label from an archive `.js` entry path:
/// the file stem, lowercased, with trailing `-partN` shard suffixes stripped
/// so `tweet-part1.js` and `tweet.js` both map to `tweet`.
fn section_name(name: &str) -> String {
    let stem = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .strip_suffix(".js")
        .or_else(|| name.rsplit('/').next())
        .unwrap_or(name)
        .to_ascii_lowercase();
    // Drop a trailing `-partN` or `-part<N>` shard suffix.
    if let Some((head, tail)) = stem.rsplit_once("-part") {
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
            return head.to_string();
        }
    }
    stem
}

/// The items of an archive JSON value after stripping the wrapper. Twitter
/// archives are always arrays at the top level after stripping; fall back to
/// treating the whole value as a single item in case of an unexpected shape.
fn array_items(value: &Value) -> Vec<&Value> {
    if let Some(arr) = value.as_array() {
        return arr.iter().collect();
    }
    std::slice::from_ref(value).iter().collect()
}

// ---------------------------------------------------------------------------
// Account owner detection (for DM from_me resolution).

/// Try to find the archive owner's accountId from `account.js`. This lives at
/// `data/account.js` and has the shape:
/// `window.YTD.account.part0 = [{"account":{"accountId":"12345",...}}]`
///
/// Returns the raw string id, or `None` if the file isn't in the archive.
fn detect_owner_id(zip: &mut zip::ZipArchive<std::fs::File>, names: &[String]) -> Option<String> {
    let account_file = names
        .iter()
        .find(|n| {
            let f = n.rsplit('/').next().unwrap_or(n).to_ascii_lowercase();
            f == "account.js"
        })?;
    let mut body = String::new();
    read_entry(zip, account_file, &mut body).ok()?;
    let stripped = strip_js_wrapper(&body)?;
    let value: Value = serde_json::from_str(&stripped).ok()?;
    for item in array_items(&value) {
        let acct = item.get("account").unwrap_or(item);
        if let Some(id) = acct.get("accountId").and_then(Value::as_str) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tweet → Post mapping.

/// Parse a Twitter archive timestamp (always `"Wed Oct 10 16:00:00 +0000 2018"`)
/// into a local RFC3339 string. `None` when unparseable.
fn parse_twitter_ts(raw: &str) -> Option<String> {
    // Twitter archives use the old REST API format: "%a %b %d %H:%M:%S %z %Y"
    DateTime::parse_from_str(raw, "%a %b %d %H:%M:%S %z %Y")
        .ok()
        .map(|t| t.with_timezone(&Local).to_rfc3339())
}

/// One archive tweet object (already unwrapped from the `{"tweet":{…}}` outer
/// wrapper) → a `social` contract [`Post`].
fn tweet_to_post(obj: &Value) -> Option<Post> {
    let obj = obj.as_object()?;
    let tweet_id = obj.get("id_str").and_then(Value::as_str)?;
    let raw_ts = obj.get("created_at").and_then(Value::as_str)?;
    let ts = parse_twitter_ts(raw_ts)?;
    let full_text = obj.get("full_text").and_then(Value::as_str).unwrap_or("").to_string();

    let mut post = Post::new(SOURCE, tweet_id, ts);

    // Detect kind: retweet vs reply vs quote vs top-level post.
    //
    // Retweet: retweeted_status nested object present, OR retweeted_status_id_str
    // non-empty, OR full_text starts with "RT @" (fallback for archive versions
    // that omit the retweeted_status linkage).
    let is_retweet = obj.contains_key("retweeted_status")
        || obj
            .get("retweeted_status_id_str")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        || full_text.starts_with("RT @");
    let in_reply_to = obj
        .get("in_reply_to_status_id_str")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    // Quote tweet: not a retweet, not a reply, but carries a quoted_status_id_str
    // or is_quote_status flag. A quote tweet embeds another tweet with commentary.
    let quoted_status_id = obj
        .get("quoted_status_id_str")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let is_quote_status = obj
        .get("is_quote_status")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_quote = !is_retweet && in_reply_to.is_none() && (quoted_status_id.is_some() || is_quote_status);

    if is_retweet {
        post.kind = "repost".into();
        // Retweet: the retweeted tweet id → repost_of. The text starts with
        // "RT @user: …" — keep it for search but the kind signals it.
        let retweeted_id = obj
            .get("retweeted_status_id_str")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        if !retweeted_id.is_empty() {
            post.repost_of = retweeted_id.to_string();
        }
    } else if let Some(reply_id) = in_reply_to {
        post.kind = "reply".into();
        post.reply_to = reply_id.to_string();
        // reply screen name → extra (not in Post core)
        if let Some(sn) = obj.get("in_reply_to_screen_name").and_then(Value::as_str) {
            if !sn.is_empty() {
                post.extra.insert("reply_to_screen_name".into(), Value::String(sn.to_string()));
            }
        }
        if let Some(uid) = obj
            .get("in_reply_to_user_id_str")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            post.extra.insert("reply_to_user_id".into(), Value::String(uid.to_string()));
        }
    } else if is_quote {
        post.kind = "quote".into();
        // Quote tweet: the quoted tweet id → quote_of.
        if let Some(qid) = quoted_status_id {
            post.quote_of = qid.to_string();
        }
    } else {
        post.kind = "post".into();
    }

    // Full text (untruncated).
    if !full_text.is_empty() {
        post.text = full_text;
    }

    // URLs from entities: expand t.co shortlinks when the `expanded_url` is
    // present; stash them in extra.urls for transparency. `url` on the Post is
    // reserved for the canonical link of a link-post — use the first non-tweet
    // expanded URL if there is one.
    let url_entities = obj
        .get("entities")
        .and_then(|e| e.get("urls"))
        .and_then(Value::as_array);
    if let Some(urls) = url_entities {
        let mut link_urls: Vec<String> = Vec::new();
        for u in urls {
            if let Some(expanded) = u.get("expanded_url").and_then(Value::as_str) {
                if !expanded.is_empty()
                    && !expanded.contains("twitter.com/")
                    && !expanded.contains("t.co/")
                {
                    link_urls.push(expanded.to_string());
                }
            }
        }
        if let Some(first_url) = link_urls.first() {
            post.url = first_url.clone();
        }
        if !link_urls.is_empty() {
            post.extra.insert(
                "urls".into(),
                Value::Array(link_urls.iter().map(|u| Value::String(u.clone())).collect()),
            );
        }
    }

    // Media metadata from extended_entities (preferred) or entities.media.
    let media_arr = obj
        .get("extended_entities")
        .and_then(|e| e.get("media"))
        .or_else(|| obj.get("entities").and_then(|e| e.get("media")))
        .and_then(Value::as_array);
    if let Some(media_items) = media_arr {
        let mut media: Vec<Media> = Vec::new();
        for m in media_items {
            let media_type = m
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("media")
                .to_string();
            let url = m
                .get("media_url_https")
                .or_else(|| m.get("media_url"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let alt = m
                .get("ext_alt_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            media.push(Media { r#type: media_type, url, alt });
        }
        if !media.is_empty() {
            post.media = media;
        }
    }

    // Hashtags → tags (from entities.hashtags[].text).
    let hashtags = obj
        .get("entities")
        .and_then(|e| e.get("hashtags"))
        .and_then(Value::as_array);
    if let Some(tags) = hashtags {
        let tag_strs: Vec<String> = tags
            .iter()
            .filter_map(|h| h.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if !tag_strs.is_empty() {
            post.tags = tag_strs;
        }
    }

    // Language → lang (if present and non-empty "und").
    if let Some(lang) = obj.get("lang").and_then(Value::as_str) {
        if !lang.is_empty() && lang != "und" {
            post.lang = lang.to_string();
        }
    }

    // Overflow: every top-level key not already mapped → extra.
    let mapped = &[
        "id_str",
        "id",
        "created_at",
        "full_text",
        "text",
        "in_reply_to_status_id_str",
        "in_reply_to_status_id",
        "in_reply_to_screen_name",
        "in_reply_to_user_id_str",
        "in_reply_to_user_id",
        "retweeted_status",
        "retweeted_status_id_str",
        "quoted_status_id_str",
        "quoted_status_id",
        "is_quote_status",
        "quoted_status_permalink",
        "entities",
        "extended_entities",
        "lang",
    ];
    for (k, v) in obj {
        if !mapped.contains(&k.as_str()) {
            post.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    Some(post)
}

// ---------------------------------------------------------------------------
// DM → Message mapping.

/// One `messageCreate` object from a `dmConversation.messages[]` entry →
/// a correspondence contract [`Message`].
///
/// `msg_id` is the stable id that has already been dedupe-checked by the
/// caller (so we don't re-derive it here).
fn dm_to_message(mc: &Value, conv_id: &str, my_id: Option<&str>, msg_id: &str) -> Option<Message> {
    let obj = mc.as_object()?;
    let raw_ts = obj.get("createdAt").and_then(Value::as_str)?;
    // DM timestamps are ISO 8601 / RFC3339.
    let ts = DateTime::parse_from_rfc3339(raw_ts)
        .ok()
        .map(|t| t.with_timezone(&Local).to_rfc3339())?;

    let sender_id = obj.get("senderId").and_then(Value::as_str).unwrap_or_default();
    let from_me = my_id.is_some_and(|id| id == sender_id);

    // Expand t.co shortlinks in the message text using the urls array, if present.
    // messageCreate carries a urls array: [{url, expanded, display}].
    let raw_text = obj.get("text").and_then(Value::as_str).unwrap_or("");
    let text = expand_dm_urls(raw_text, obj.get("urls").and_then(Value::as_array));

    // mediaUrls is an array of URL strings.
    let attachments: Vec<AttachmentMeta> = obj
        .get("mediaUrls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|url| {
                    let name = url.rsplit('/').next().unwrap_or("").to_string();
                    AttachmentMeta {
                        name: if name.is_empty() { "attachment".to_string() } else { name },
                        mime: String::new(),
                        bytes: 0,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let mut m = Message::new(CORR_SOURCE, ts);
    m.guid = msg_id.to_string();
    m.chat = conv_id.to_string();
    m.from_me = from_me;
    if !from_me {
        m.sender = sender_id.to_string();
    }
    m.text = text;
    m.attachments = attachments;
    m.service = "X".to_string();
    Some(m)
}

/// Expand t.co shortlinks in a DM message text. The archive provides a `urls`
/// array alongside the text: `[{"url":"https://t.co/…","expanded":"https://…","display":"…"}]`.
/// We replace each t.co occurrence in the text with its expanded form.
fn expand_dm_urls(text: &str, urls: Option<&Vec<Value>>) -> String {
    let Some(urls) = urls else { return text.to_string() };
    if urls.is_empty() { return text.to_string() }
    let mut result = text.to_string();
    for entry in urls {
        if let (Some(short), Some(expanded)) = (
            entry.get("url").and_then(Value::as_str),
            entry.get("expanded").and_then(Value::as_str),
        ) {
            if !short.is_empty() && !expanded.is_empty() {
                result = result.replace(short, expanded);
            }
        }
    }
    result
}

/// A `reactionCreate` entry → a correspondence Message with `kind:"reaction"`.
/// The archive shape is: `{"reactionCreate":{"senderId":"…","reactionKey":"…",
/// "eventId":"…","createdAt":"…","conversationId":"…"}}`.
fn dm_reaction_to_message(rc: &Value, conv_id: &str, my_id: Option<&str>, msg_id: &str) -> Option<Message> {
    let obj = rc.as_object()?;
    let raw_ts = obj.get("createdAt").and_then(Value::as_str)?;
    let ts = DateTime::parse_from_rfc3339(raw_ts)
        .ok()
        .map(|t| t.with_timezone(&Local).to_rfc3339())?;

    let sender_id = obj.get("senderId").and_then(Value::as_str).unwrap_or_default();
    let from_me = my_id.is_some_and(|id| id == sender_id);
    let reaction_key = obj.get("reactionKey").and_then(Value::as_str).unwrap_or("").to_string();

    let mut m = Message::new(CORR_SOURCE, ts);
    m.guid = msg_id.to_string();
    m.chat = conv_id.to_string();
    m.from_me = from_me;
    if !from_me {
        m.sender = sender_id.to_string();
    }
    m.kind = "reaction".to_string();
    m.reaction = reaction_key;
    // eventId is the id of the message being reacted to.
    if let Some(event_id) = obj.get("eventId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m.reply_to = event_id.to_string();
    }
    m.service = "X".to_string();
    Some(m)
}

/// A DM group event entry (joinConversation, participantsJoin, participantsLeave,
/// conversationNameUpdate, welcomeMessageCreate) → a correspondence Message with
/// `kind:"event"` so the full thread history is preserved.
fn dm_event_to_message(entry: &Value, entry_type: &str, conv_id: &str, msg_id: &str) -> Option<Message> {
    let obj = entry.as_object()?;
    // Best-effort timestamp: createdAt or initiatedAt.
    let raw_ts = obj.get("createdAt")
        .or_else(|| obj.get("initiatedAt"))
        .and_then(Value::as_str)?;
    let ts = DateTime::parse_from_rfc3339(raw_ts)
        .ok()
        .map(|t| t.with_timezone(&Local).to_rfc3339())?;

    // Build a human-readable text summary for the event.
    let text = match entry_type {
        "joinConversation" | "participantsJoin" => {
            let participants = obj
                .get("userIds")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
                })
                .unwrap_or_default();
            if participants.is_empty() {
                format!("[{entry_type}]")
            } else {
                format!("[{entry_type}: {participants}]")
            }
        }
        "participantsLeave" => {
            let participants = obj
                .get("userIds")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
                })
                .unwrap_or_default();
            if participants.is_empty() {
                "[participantsLeave]".to_string()
            } else {
                format!("[participantsLeave: {participants}]")
            }
        }
        "conversationNameUpdate" => {
            let new_name = obj.get("name").and_then(Value::as_str).unwrap_or("(unnamed)");
            format!("[conversationNameUpdate: {new_name}]")
        }
        _ => format!("[{entry_type}]"),
    };

    let mut m = Message::new(CORR_SOURCE, ts);
    m.guid = msg_id.to_string();
    m.chat = conv_id.to_string();
    m.kind = "event".to_string();
    m.text = text;
    m.service = "X".to_string();
    Some(m)
}

// ---------------------------------------------------------------------------
// Raw section helpers.

/// A stable content hash of a raw item, for re-import dedupe.
fn content_hash(item: &Value) -> String {
    let mut h = Sha256::new();
    h.update(item.to_string().as_bytes());
    format!("{:x}", h.finalize())
}

/// Guids already stored in a raw section file, for re-runnable imports.
fn load_raw_section_guids(vault: &Vault, section: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(path) = vault.resolve(&format!("{RAW_DIR}/{section}.jsonl")) else {
        return out;
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        return out;
    };
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                out.insert(g.to_string());
            }
        }
    }
    out
}

/// Append raw rows to `social/x-twitter/raw/<section>.jsonl`.
fn append_raw_section(vault: &Vault, section: &str, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let rel = format!("{RAW_DIR}/{section}.jsonl");
    let path = vault.resolve(&rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {rel}"))?;
    for row in rows {
        writeln!(f, "{}", serde_json::to_string(row)?)?;
    }
    Ok(())
}

/// Read one zip entry's bytes into `body` as a string.
fn read_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str, body: &mut String) -> Result<()> {
    body.clear();
    zip.by_name(name)
        .with_context(|| format!("entry {name}"))?
        .read_to_string(body)
        .with_context(|| format!("reading {name}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-xtwitter-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn ack() -> BTreeMap<String, String> {
        [("acknowledge".to_string(), "yes".to_string())].into()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &ack(), &mut |_| {}).unwrap()
    }

    // ── Fixture helpers ──────────────────────────────────────────────────────

    /// Build a minimal X archive ZIP with:
    /// - `data/account.js` (owner id 12345)
    /// - `data/tweet.js`   (two tweets: one plain, one reply)
    /// - `data/direct-messages.js` (one 1:1 DM thread, two messages)
    /// - `data/like.js`    (two liked tweet references)
    fn archive_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-xtwitter-archive-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // account.js
        z.start_file("data/account.js", opts).unwrap();
        z.write_all(
            b"window.YTD.account.part0 = [\n\
              {\"account\":{\"accountId\":\"12345\",\"username\":\"testuser\"}}\n\
              ]\n",
        )
        .unwrap();

        // tweet.js — two tweets
        // Tweet 1: a plain post (no reply, no retweet, has hashtag, lang)
        // Tweet 2: a reply with media
        z.start_file("data/tweet.js", opts).unwrap();
        z.write_all(
            b"window.YTD.tweets.part0 = [\n\
              {\"tweet\":{\"id_str\":\"1001\",\"created_at\":\"Mon Jun 10 14:03:01 +0000 2024\",\
               \"full_text\":\"Hello #world from Trove\",\
               \"lang\":\"en\",\
               \"entities\":{\"hashtags\":[{\"text\":\"world\"}],\"urls\":[]},\
               \"extended_entities\":{}}},\n\
              {\"tweet\":{\"id_str\":\"1002\",\"created_at\":\"Tue Jun 11 09:00:00 +0000 2024\",\
               \"full_text\":\"@someone nice pic\",\
               \"in_reply_to_status_id_str\":\"999\",\
               \"in_reply_to_screen_name\":\"someone\",\
               \"in_reply_to_user_id_str\":\"7777\",\
               \"entities\":{\"hashtags\":[],\"media\":[{\"type\":\"photo\",\
                 \"media_url_https\":\"https://pbs.twimg.com/media/abc.jpg\",\
                 \"ext_alt_text\":\"a photo\"}],\"urls\":[]},\
               \"extended_entities\":{\"media\":[{\"type\":\"photo\",\
                 \"media_url_https\":\"https://pbs.twimg.com/media/abc.jpg\",\
                 \"ext_alt_text\":\"a photo\"}]}}}\n\
              ]\n",
        )
        .unwrap();

        // direct-messages.js — one 1:1 thread, two messages
        z.start_file("data/direct-messages.js", opts).unwrap();
        z.write_all(
            b"window.YTD.direct_messages.part0 = [\n\
              {\"dmConversation\":{\
                \"conversationId\":\"12345-99999\",\
                \"messages\":[\
                  {\"messageCreate\":{\"id\":\"dm1\",\"senderId\":\"12345\",\
                    \"recipientId\":\"99999\",\"text\":\"hello from me\",\
                    \"createdAt\":\"2024-06-10T14:00:00.000Z\",\
                    \"mediaUrls\":[]}},\
                  {\"messageCreate\":{\"id\":\"dm2\",\"senderId\":\"99999\",\
                    \"recipientId\":\"12345\",\"text\":\"hey!\",\
                    \"createdAt\":\"2024-06-10T14:01:00.000Z\",\
                    \"mediaUrls\":[\"https://ton.twitter.com/media/img.jpg\"]}}\
                ]}}\n\
              ]\n",
        )
        .unwrap();

        // like.js — two likes (raw only, no contract)
        z.start_file("data/like.js", opts).unwrap();
        z.write_all(
            b"window.YTD.like.part0 = [\n\
              {\"like\":{\"tweetId\":\"5000\",\"fullText\":\"Liked tweet text\",\"expandedUrl\":\"https://twitter.com/x/status/5000\"}},\n\
              {\"like\":{\"tweetId\":\"5001\",\"fullText\":\"Another liked tweet\",\"expandedUrl\":\"https://twitter.com/x/status/5001\"}}\n\
              ]\n",
        )
        .unwrap();

        z.finish().unwrap();
        path
    }

    /// A retweet archive ZIP (one tweet that is a retweet).
    fn retweet_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-xtwitter-rt-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("data/account.js", opts).unwrap();
        z.write_all(
            b"window.YTD.account.part0 = [\n\
              {\"account\":{\"accountId\":\"12345\",\"username\":\"testuser\"}}\n\
              ]\n",
        )
        .unwrap();
        z.start_file("data/tweet.js", opts).unwrap();
        z.write_all(
            b"window.YTD.tweets.part0 = [\n\
              {\"tweet\":{\"id_str\":\"2001\",\"created_at\":\"Wed Jun 12 08:00:00 +0000 2024\",\
               \"full_text\":\"RT @origuser: original tweet text\",\
               \"retweeted_status_id_str\":\"1500\",\
               \"entities\":{\"hashtags\":[],\"urls\":[]}}}\n\
              ]\n",
        )
        .unwrap();
        z.finish().unwrap();
        path
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[test]
    fn strip_js_wrapper_extracts_json_array() {
        let body = "window.YTD.tweets.part0 = [\n  {\"tweet\":{}}\n]\n";
        let stripped = strip_js_wrapper(body).unwrap();
        assert!(stripped.starts_with('['), "must start with '['");
        let v: Value = serde_json::from_str(&stripped).expect("must parse as JSON");
        assert!(v.is_array());
        let _ = fs::remove_file("/tmp/dummy");
    }

    #[test]
    fn strip_js_wrapper_none_for_empty() {
        assert!(strip_js_wrapper("window.YTD.x = \n").is_none());
        assert!(strip_js_wrapper("").is_none());
    }

    #[test]
    fn section_name_strips_part_suffix() {
        assert_eq!(section_name("data/tweets-part1.js"), "tweets");
        assert_eq!(section_name("data/tweet.js"), "tweet");
        assert_eq!(section_name("data/direct-messages.js"), "direct-messages");
        assert_eq!(section_name("data/like.js"), "like");
    }

    #[test]
    fn imports_tweets_to_social_contract() {
        let v = temp_vault("tweets");
        let zip = archive_zip("tweets");
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("tweets"), Some(&2), "two tweets: {}", out.headline);

        // 1001: 2024-06-10, 1002: 2024-06-11 → both in 2024-06
        let raw = fs::read_to_string(v.root().join("social/x-twitter/2024-06.jsonl")).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(rows.len(), 2, "two Post rows");

        // Tweet 1: plain post
        let t1 = rows.iter().find(|p| p.guid == "1001").unwrap();
        assert_eq!(t1.kind, "post");
        assert_eq!(t1.source, SOURCE);
        assert_eq!(t1.text, "Hello #world from Trove");
        assert_eq!(t1.lang, "en");
        assert_eq!(t1.tags, vec!["world"]);

        // Tweet 2: reply
        let t2 = rows.iter().find(|p| p.guid == "1002").unwrap();
        assert_eq!(t2.kind, "reply");
        assert_eq!(t2.reply_to, "999");
        assert_eq!(t2.extra.get("reply_to_screen_name"), Some(&json!("someone")));
        assert_eq!(t2.media.len(), 1);
        assert_eq!(t2.media[0].r#type, "photo");
        assert_eq!(t2.media[0].alt, "a photo");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn imports_tweets_to_raw_layer() {
        let v = temp_vault("raw");
        let zip = archive_zip("raw");
        run(&v, &zip);

        let raw = fs::read_to_string(v.root().join("social/x-twitter/raw/tweets.jsonl")).unwrap();
        let rows: Vec<Value> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(rows.len(), 2, "both tweets in raw layer");
        assert!(rows.iter().all(|r| r["section"] == json!("tweets")));
        assert!(rows.iter().all(|r| r.get("raw").is_some()), "full-fidelity raw kept");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn imports_dms_to_correspondence() {
        let v = temp_vault("dms");
        let zip = archive_zip("dms");
        run(&v, &zip);

        let day = v.correspondence_timeline("2024-06-10").unwrap();
        assert_eq!(day.len(), 2, "two DM messages");

        let mine = day.iter().find(|m| m.guid == "dm1").unwrap();
        assert!(mine.from_me, "sender=12345=owner → from_me");
        assert_eq!(mine.chat, "12345-99999");
        assert_eq!(mine.service, "X");
        assert_eq!(mine.text, "hello from me");
        assert_eq!(mine.sender, "", "sender empty when from_me");

        let theirs = day.iter().find(|m| m.guid == "dm2").unwrap();
        assert!(!theirs.from_me, "sender=99999≠owner → received");
        assert_eq!(theirs.sender, "99999");
        assert_eq!(theirs.attachments.len(), 1, "media url → attachment metadata");
        assert_eq!(theirs.attachments[0].name, "img.jpg");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn likes_go_to_raw_layer() {
        let v = temp_vault("likes");
        let zip = archive_zip("likes");
        let out = run(&v, &zip);
        assert!(out.counts.get("raw").unwrap() >= &2, "at least 2 raw (likes + tweets): {}", out.headline);

        let raw = fs::read_to_string(v.root().join("social/x-twitter/raw/like.jsonl")).unwrap();
        let rows: Vec<Value> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(rows.len(), 2, "two like raw rows");
        assert_eq!(rows[0]["section"], json!("like"));

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn retweet_gets_kind_repost_and_repost_of() {
        let v = temp_vault("retweet");
        let zip = retweet_zip("retweet");
        run(&v, &zip);

        let raw = fs::read_to_string(v.root().join("social/x-twitter/2024-06.jsonl")).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let rt = rows.iter().find(|p| p.guid == "2001").unwrap();
        assert_eq!(rt.kind, "repost");
        assert_eq!(rt.repost_of, "1500");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reimport_dedupes() {
        let v = temp_vault("reimport");
        let zip = archive_zip("reimport");

        run(&v, &zip);
        let tweets_before =
            fs::read_to_string(v.root().join("social/x-twitter/2024-06.jsonl")).unwrap();
        let corr_before =
            fs::read_to_string(v.root().join("correspondence/x-twitter/2024-06.jsonl")).unwrap();

        let out2 = run(&v, &zip);
        assert_eq!(out2.counts.get("tweets"), Some(&0), "no new tweets");
        assert_eq!(out2.counts.get("dms"), Some(&0), "no new dms");
        assert!(*out2.counts.get("duplicates").unwrap() >= 2);

        let tweets_after =
            fs::read_to_string(v.root().join("social/x-twitter/2024-06.jsonl")).unwrap();
        let corr_after =
            fs::read_to_string(v.root().join("correspondence/x-twitter/2024-06.jsonl")).unwrap();
        assert_eq!(tweets_before, tweets_after, "tweets unchanged on re-import");
        assert_eq!(corr_before, corr_after, "dms unchanged on re-import");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn back_compat_post_deserializes() {
        // A sparse social line (only required fields) must round-trip.
        let line = r#"{"ts":"2024-06-10T07:03:01-07:00","source":"x-twitter","guid":"1001","future_field":"x"}"#;
        let p: Post = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "1001");
        assert_eq!(p.kind, "");
        assert!(p.text.is_empty());
    }

    #[test]
    fn back_compat_message_deserializes() {
        // A sparse correspondence line must round-trip.
        let line = r#"{"ts":"2024-06-10T07:03:01-07:00","source":"x-twitter","chat":"12345-99999","from_me":true,"kind":"message","text":"hi"}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hi");
        assert!(m.from_me);
    }

    // ── New fixtures ─────────────────────────────────────────────────────────

    /// A quote-tweet archive ZIP: one tweet that quotes another via
    /// `quoted_status_id_str` + `is_quote_status:true`, with no retweeted_status
    /// and no in_reply_to (so it is a top-level quote, not a reply-quote).
    fn quote_tweet_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-xtwitter-quote-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("data/account.js", opts).unwrap();
        z.write_all(
            b"window.YTD.account.part0 = [\n\
              {\"account\":{\"accountId\":\"12345\",\"username\":\"testuser\"}}\n\
              ]\n",
        ).unwrap();
        z.start_file("data/tweet.js", opts).unwrap();
        z.write_all(
            b"window.YTD.tweets.part0 = [\n\
              {\"tweet\":{\"id_str\":\"3001\",\"created_at\":\"Thu Jun 13 10:00:00 +0000 2024\",\
               \"full_text\":\"This is my commentary on the original tweet https://t.co/example\",\
               \"is_quote_status\":true,\
               \"quoted_status_id_str\":\"2500\",\
               \"quoted_status_permalink\":{\"url\":\"https://t.co/example\",\"expanded\":\"https://twitter.com/origuser/status/2500\",\"display\":\"twitter.com/origuser/status/2500\"},\
               \"lang\":\"en\",\
               \"entities\":{\"hashtags\":[],\"urls\":[]}}}\n\
              ]\n",
        ).unwrap();
        z.finish().unwrap();
        path
    }

    /// A retweet archive ZIP where the tweet carries ONLY the "RT @" prefix
    /// and no retweeted_status or retweeted_status_id_str — exercises the
    /// full_text fallback for retweet detection.
    fn rt_prefix_only_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-xtwitter-rtprefix-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("data/account.js", opts).unwrap();
        z.write_all(
            b"window.YTD.account.part0 = [\n\
              {\"account\":{\"accountId\":\"12345\",\"username\":\"testuser\"}}\n\
              ]\n",
        ).unwrap();
        z.start_file("data/tweet.js", opts).unwrap();
        z.write_all(
            b"window.YTD.tweets.part0 = [\n\
              {\"tweet\":{\"id_str\":\"4001\",\"created_at\":\"Fri Jun 14 08:00:00 +0000 2024\",\
               \"full_text\":\"RT @origuser: some reposted content here\",\
               \"entities\":{\"hashtags\":[],\"urls\":[]}}}\n\
              ]\n",
        ).unwrap();
        z.finish().unwrap();
        path
    }

    /// A DM archive ZIP where the DM text contains a t.co shortlink with
    /// expansion provided in the `urls` array field of `messageCreate`.
    fn dm_with_url_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-xtwitter-dmurl-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("data/account.js", opts).unwrap();
        z.write_all(
            b"window.YTD.account.part0 = [\n\
              {\"account\":{\"accountId\":\"12345\",\"username\":\"testuser\"}}\n\
              ]\n",
        ).unwrap();
        z.start_file("data/direct-messages.js", opts).unwrap();
        z.write_all(
            b"window.YTD.direct_messages.part0 = [\n\
              {\"dmConversation\":{\
                \"conversationId\":\"12345-77777\",\
                \"messages\":[\
                  {\"messageCreate\":{\"id\":\"dmu1\",\"senderId\":\"12345\",\
                    \"recipientId\":\"77777\",\
                    \"text\":\"Check this out https://t.co/abc123\",\
                    \"createdAt\":\"2024-06-15T10:00:00.000Z\",\
                    \"mediaUrls\":[],\
                    \"urls\":[{\"url\":\"https://t.co/abc123\",\
                               \"expanded\":\"https://example.com/real-link\",\
                               \"display\":\"example.com/real-link\"}]}}\
                ]}}\n\
              ]\n",
        ).unwrap();
        z.finish().unwrap();
        path
    }

    /// A DM archive ZIP with a reactionCreate and a joinConversation entry
    /// alongside a messageCreate, to test non-messageCreate DM handling.
    fn dm_events_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-xtwitter-dmevents-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("data/account.js", opts).unwrap();
        z.write_all(
            b"window.YTD.account.part0 = [\n\
              {\"account\":{\"accountId\":\"12345\",\"username\":\"testuser\"}}\n\
              ]\n",
        ).unwrap();
        z.start_file("data/direct-message-group-conversations.js", opts).unwrap();
        z.write_all(
            b"window.YTD.direct_messages_group.part0 = [\n\
              {\"dmConversation\":{\
                \"conversationId\":\"group-abc\",\
                \"messages\":[\
                  {\"messageCreate\":{\"id\":\"gmsg1\",\"senderId\":\"99999\",\
                    \"text\":\"Hello group!\",\
                    \"createdAt\":\"2024-06-16T09:00:00.000Z\",\
                    \"mediaUrls\":[]}},\
                  {\"reactionCreate\":{\"senderId\":\"12345\",\"reactionKey\":\"funny\",\
                    \"eventId\":\"gmsg1\",\"createdAt\":\"2024-06-16T09:01:00.000Z\"}},\
                  {\"joinConversation\":{\"initiatingUserId\":\"99999\",\
                    \"userIds\":[\"12345\",\"77777\"],\
                    \"createdAt\":\"2024-06-16T08:59:00.000Z\"}}\
                ]}}\n\
              ]\n",
        ).unwrap();
        z.finish().unwrap();
        path
    }

    // ── Tests for new fixtures ───────────────────────────────────────────────

    #[test]
    fn quote_tweet_gets_kind_quote_and_quote_of() {
        let v = temp_vault("quote");
        let zip = quote_tweet_zip("quote");
        run(&v, &zip);

        let raw = fs::read_to_string(v.root().join("social/x-twitter/2024-06.jsonl")).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let qt = rows.iter().find(|p| p.guid == "3001").unwrap();
        assert_eq!(qt.kind, "quote", "should be kind:quote, not kind:post");
        assert_eq!(qt.quote_of, "2500", "quote_of should hold the quoted tweet id");
        assert_eq!(qt.repost_of, "", "repost_of should be empty for a quote tweet");
        assert_eq!(qt.reply_to, "", "reply_to should be empty for a quote tweet");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn retweet_with_only_rt_prefix_gets_kind_repost() {
        // Archive record with no retweeted_status / retweeted_status_id_str —
        // the "RT @" full_text fallback must still classify it as a repost.
        let v = temp_vault("rtprefix");
        let zip = rt_prefix_only_zip("rtprefix");
        run(&v, &zip);

        let raw = fs::read_to_string(v.root().join("social/x-twitter/2024-06.jsonl")).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let rt = rows.iter().find(|p| p.guid == "4001").unwrap();
        assert_eq!(rt.kind, "repost", "RT @ prefix should yield kind:repost");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn dm_url_expansion_replaces_tco_links() {
        let v = temp_vault("dmurl");
        let zip = dm_with_url_zip("dmurl");
        run(&v, &zip);

        let day = v.correspondence_timeline("2024-06-15").unwrap();
        let msg = day.iter().find(|m| m.guid == "dmu1").unwrap();
        assert!(
            msg.text.contains("https://example.com/real-link"),
            "expanded URL should appear in text, got: {}",
            msg.text
        );
        assert!(
            !msg.text.contains("https://t.co/abc123"),
            "raw t.co link should be replaced, got: {}",
            msg.text
        );

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn dm_non_message_entries_are_preserved() {
        // A group DM archive with messageCreate, reactionCreate, and joinConversation
        // entries: all three should land in the correspondence stream, none dropped.
        let v = temp_vault("dmevents");
        let zip = dm_events_zip("dmevents");
        let out = run(&v, &zip);
        assert!(
            *out.counts.get("dms").unwrap() >= 3,
            "all 3 DM entries (message + reaction + join event) should be written: {}",
            out.headline
        );

        let day = v.correspondence_timeline("2024-06-16").unwrap();
        assert!(day.len() >= 3, "at least 3 correspondence entries for the group DM");

        // The messageCreate
        let msg = day.iter().find(|m| m.guid == "gmsg1").unwrap();
        assert_eq!(msg.kind, "message");
        assert_eq!(msg.text, "Hello group!");

        // The reactionCreate should be kind:reaction
        let reaction = day.iter().find(|m| m.kind == "reaction").unwrap();
        assert_eq!(reaction.reaction, "funny", "reactionKey should map to reaction field");
        assert_eq!(reaction.reply_to, "gmsg1", "eventId should map to reply_to");

        // The joinConversation should be kind:event
        let join = day.iter().find(|m| m.kind == "event").unwrap();
        assert!(join.text.contains("joinConversation"), "event text should name the entry type");

        let _ = fs::remove_file(zip);
    }
}
