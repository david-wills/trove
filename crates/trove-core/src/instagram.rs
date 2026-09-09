//! Instagram — file-import of posts, stories, reels, and DMs from Meta's
//! Accounts Center JSON export (`accountscenter.meta.com` → Export your
//! information → JSON format). The same ZIP bundles Threads content, which
//! is routed to `social/threads/` in the same pass.
//!
//! ## What lands where
//!
//! - **Posts / stories / reels** → social contract rows
//!   (`social/instagram/YYYY-MM.jsonl`, [`crate::social::Post`], `kind:"post"`
//!   / `"story"` / `"reel"`). Also kept full-fidelity in
//!   `social/instagram/raw/posts.jsonl`.
//! - **DMs** (`messages/inbox/<thread>/message_N.json`) → correspondence
//!   contract rows (`correspondence/instagram/YYYY-MM.jsonl`,
//!   [`crate::correspondence::Message`]). The vault owner is inferred from an
//!   optional `owner_name` param; without it `from_me` is `false` for every
//!   message (still useful for reading the thread).
//! - **Threads** (`threads_and_replies/threads_and_replies.json` or
//!   `threads_and_replies.json`) → social contract rows for the `threads`
//!   provider (`social/threads/YYYY-MM.jsonl`).
//! - **Everything else** (followers, following, likes, comments,
//!   ad interactions, …) → raw layer
//!   `social/instagram/raw/<section>.jsonl`, full-fidelity.
//!
//! ## Meta mojibake
//!
//! Instagram DYI strings carry the same UTF-8-as-Latin-1 encoding error as
//! Facebook exports. Every string parsed from the ZIP is repaired through
//! [`crate::meta_encoding::fix_value`]; pure ASCII and already-correct emoji
//! are untouched.
//!
//! ## Deduplication
//!
//! Neither posts nor DMs carry a stable source id in the export. Posts use a
//! length-prefixed sha256 over (unix timestamp + caption text + file uri);
//! DMs use a sha256 over (thread title + sender + timestamp_ms + content).
//! Re-importing the same or a newer export is safe: already-held guids are
//! skipped.
//!
//! ## Photos / stories contract
//!
//! The Phase 3 photos-metadata contract is not yet ratified — photo metadata
//! stays in the raw layer (`social/instagram/raw/`) for now. Stories and reels
//! ship as `Post` rows (`kind:"story"` / `kind:"reel"`) since they carry the
//! same timestamp+caption+media shape as posts.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};

use crate::correspondence::Message;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::meta_encoding::fix_value;
use crate::registry::{Behavior, ImportOutcome, ImportParam, ImportSpec, IntegrationDef};
use crate::social::{Media, Post};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "instagram";
const THREADS_SOURCE: &str = "threads";
const SOCIAL_DIR: &str = "social/instagram";
const THREADS_DIR: &str = "social/threads";
const RAW_DIR: &str = "social/instagram/raw";
const CORR_SOURCE: &str = "instagram";

// ---------------------------------------------------------------------------
// Def + ImportSpec.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(SOCIAL_DIR))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join(format!("correspondence/{CORR_SOURCE}"))))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "instagram",
        name: "Instagram",
        kind: IntegrationKind::Import,
        // 🔒 DMs are intimate — opt-in only.
        default_on: false,
        description: "Import your Instagram posts, stories, reels, and direct messages from \
                      Meta's Accounts Center JSON export. Posts join your social stream; DMs \
                      join your correspondence vault. Threads content in the same ZIP is also \
                      parsed. Re-runnable — re-importing a newer export never duplicates.",
        domain: "social",
        vault_path: "social/instagram/",
        toggleable: false,
        setup: &[
            "Go to accountscenter.meta.com → Your information and permissions → \
             Download your information → Download or transfer information → \
             Some of your information → select categories → Download to device → \
             JSON format. A link is emailed within hours to 48 hours.",
            "Drop the ZIP here. This export contains intimate categories (DMs, \
             followers, ad interactions); import only when you intend to store \
             that data in your private vault. Download links expire after 4 days \
             — import promptly. Media files are never copied; only metadata.",
        ],
        caveats: "Meta exports take up to 48 hours and the download link expires after 4 days. \
                  Neither posts nor DMs carry a stable source id — deduplication uses a content \
                  hash (timestamp + text), so editing a caption post-export looks like a new post \
                  on the next import.",
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
            // 🔒 Opt-in acknowledgement for intimate categories.
            key: "acknowledge",
            label: "Privacy acknowledgement",
            placeholder: "Type 'yes' to confirm you want this intimate export stored in your vault",
            required: true,
        },
        ImportParam {
            // Owner name lets us mark from_me correctly on DMs.
            key: "owner_name",
            label: "Your Instagram display name",
            placeholder: "e.g. Jane Smith — used to mark your own DMs as sent (optional)",
            required: false,
        },
    ],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Stats.

#[derive(Default)]
struct Stats {
    posts: u64,
    threads_posts: u64,
    messages: u64,
    raw: u64,
    duplicates: u64,
    sections: HashSet<String>,
}

// ---------------------------------------------------------------------------
// Import.

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let owner_name = params.get("owner_name").filter(|s| !s.is_empty()).cloned();

    // Load dedupe sets.
    let post_stream = vault.stream(SOCIAL_DIR, Partition::Month);
    let mut seen_posts: HashSet<String> = HashSet::new();
    for key in post_stream.partitions()? {
        for p in post_stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen_posts.insert(p.guid);
            }
        }
    }
    let threads_stream = vault.stream(THREADS_DIR, Partition::Month);
    let mut seen_threads: HashSet<String> = HashSet::new();
    for key in threads_stream.partitions()? {
        for p in threads_stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen_threads.insert(p.guid);
            }
        }
    }
    let mut seen_msgs: HashSet<String> = vault.correspondence_guids(CORR_SOURCE)?;

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this an Instagram/Meta export ZIP?", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    let mut stats = Stats::default();
    let mut posts: Vec<Post> = Vec::new();
    let mut threads_posts: Vec<Post> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    let mut raw_by_section: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut raw_seen: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    for name in &names {
        let lower = name.replace('\\', "/");
        if !lower.ends_with(".json") {
            continue; // media files — never copied.
        }

        let mut body = String::new();
        if read_entry(&mut zip, name, &mut body).is_err() {
            continue;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        // Repair Meta mojibake across every string in this section.
        fix_value(&mut value);

        if is_dm_file(&lower) {
            // Direct message thread — route to correspondence.
            let thread_title = thread_title_from_path(&lower);
            parse_dm_thread(
                &value,
                &thread_title,
                owner_name.as_deref(),
                &mut seen_msgs,
                &mut messages,
                &mut stats,
            );
        } else if is_threads_file(&lower) {
            // Threads content — route to social/threads/.
            parse_threads(
                &value,
                &mut seen_threads,
                &mut threads_posts,
                &mut stats,
            );
        } else if is_posts_file(&lower) {
            // Posts / stories / reels — route to social/instagram/ contract
            // AND the raw layer (full fidelity).
            let kind = post_kind_from_path(&lower);
            let section = section_name(&lower);
            let seen_raw = raw_seen
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, &section));
            let raw_bucket = raw_by_section.entry(section.clone()).or_default();

            for raw_post in posts_array(&value) {
                // Raw copy, full fidelity.
                let raw_guid = content_hash(raw_post);
                if seen_raw.insert(raw_guid.clone()) {
                    raw_bucket.push(
                        json!({"section": section, "guid": raw_guid, "raw": raw_post.clone()}),
                    );
                    stats.raw += 1;
                    stats.sections.insert(section.clone());
                }
                // Normalized contract row.
                let Some(post) = post_from_value(raw_post, &kind) else { continue };
                if !seen_posts.insert(post.guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                posts.push(post);
                stats.posts += 1;
            }
        } else {
            // Any other section (followers, following, likes, ads, …) → raw.
            let section = section_name(&lower);
            let seen = raw_seen
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, &section));
            let bucket = raw_by_section.entry(section.clone()).or_default();
            for item in section_items(&value) {
                let guid = content_hash(&item);
                if !seen.insert(guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                bucket.push(json!({"section": section, "guid": guid, "raw": item}));
                stats.raw += 1;
                stats.sections.insert(section.clone());
            }
        }
    }

    // Write social contract rows for posts.
    post_stream.append(&posts, |p| &p.ts)?;
    // Write social contract rows for threads.
    threads_stream.append(&threads_posts, |p| &p.ts)?;
    // Write correspondence contract rows for DMs.
    vault.append_messages(&messages)?;
    // Write raw sections.
    for (section, rows) in &raw_by_section {
        if rows.is_empty() {
            continue;
        }
        append_raw_section(vault, &section, rows)?;
    }

    let total = stats.posts + stats.threads_posts + stats.messages + stats.raw;
    progress(ImportProgress { records: total, percent: 100.0 });

    let headline = format!(
        "{} posts imported, {} Threads posts, {} DM messages, {} raw items across {} sections, {} duplicates skipped",
        stats.posts,
        stats.threads_posts,
        stats.messages,
        stats.raw,
        stats.sections.len(),
        stats.duplicates,
    );

    Ok(ImportOutcome {
        headline,
        counts: [
            ("posts", stats.posts),
            ("threads_posts", stats.threads_posts),
            ("messages", stats.messages),
            ("raw", stats.raw),
            ("sections", stats.sections.len() as u64),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// ZIP entry classification.

/// DM inbox thread file: `messages/inbox/<thread>/message_N.json` (also
/// `messages/archived_threads/`, `messages/filtered_threads/`).
fn is_dm_file(lower: &str) -> bool {
    lower.contains("messages/inbox/")
        || lower.contains("messages/archived_threads/")
        || lower.contains("messages/filtered_threads/")
        || lower.contains("messages/e2ee_cutover/")
}

/// `threads_and_replies/threads_and_replies.json` or
/// `threads_and_replies.json` (layout may vary).
fn is_threads_file(lower: &str) -> bool {
    lower.ends_with("threads_and_replies.json")
}

/// `content/posts_1.json`, `content/stories.json`, `content/reels.json`,
/// `content/reels_1.json`, etc. Matches the `content/` prefix containing
/// media metadata arrays.
fn is_posts_file(lower: &str) -> bool {
    let file = lower.rsplit('/').next().unwrap_or(lower);
    // Posts in content/ only — avoid false matches on other sections.
    lower.contains("content/")
        && (file.starts_with("posts")
            || file.starts_with("stories")
            || file.starts_with("reels"))
}

/// Infer the `Post::kind` from the entry path.
fn post_kind_from_path(lower: &str) -> String {
    let file = lower.rsplit('/').next().unwrap_or(lower);
    if file.starts_with("stories") {
        "story".into()
    } else if file.starts_with("reels") {
        "reel".into()
    } else {
        "post".into()
    }
}

/// The thread title from a DM path:
/// `messages/inbox/<thread_title>/message_1.json` → `<thread_title>`.
fn thread_title_from_path(lower: &str) -> String {
    // Split on "/" and take the segment after "inbox", "archived_threads", etc.
    let parts: Vec<&str> = lower.split('/').collect();
    for (i, p) in parts.iter().enumerate() {
        if matches!(*p, "inbox" | "archived_threads" | "filtered_threads" | "e2ee_cutover") {
            if let Some(title) = parts.get(i + 1) {
                // Strip any trailing numeric suffix from the folder name.
                return title.to_string();
            }
        }
    }
    // Fallback: the second-to-last path component.
    let n = parts.len();
    if n >= 2 { parts[n - 2].to_string() } else { "unknown".to_string() }
}

/// Human-readable section label from a ZIP entry path — used as both the
/// section name in raw rows and as the raw file name. Strips shard/version
/// suffixes (`_1`, `_v2`, …) so multiple shards collapse to one file.
fn section_name(lower: &str) -> String {
    let stem = lower
        .rsplit('/')
        .next()
        .unwrap_or(lower)
        .strip_suffix(".json")
        .unwrap_or(lower)
        .to_string();
    strip_version_suffix(&stem).to_string()
}

fn strip_version_suffix(stem: &str) -> &str {
    if let Some((head, tail)) = stem.rsplit_once('_') {
        let digits = tail.strip_prefix('v').unwrap_or(tail);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return head;
        }
    }
    stem
}

// ---------------------------------------------------------------------------
// Posts / stories / reels → social contract.

/// The array of post objects from a `posts_*.json`, `stories.json`, or
/// `reels*.json` value. Instagram exports are typically a top-level JSON
/// array; fall back to searching for an object-wrapped array.
fn posts_array(value: &Value) -> Vec<&Value> {
    if let Some(arr) = value.as_array() {
        return arr.iter().collect();
    }
    // Some export shapes wrap the array in an object (e.g. `{"media": [...]}`).
    if let Some(obj) = value.as_object() {
        for v in obj.values() {
            if let Some(arr) = v.as_array() {
                if !arr.is_empty() {
                    return arr.iter().collect();
                }
            }
        }
    }
    Vec::new()
}

/// One Instagram DYI post object → a `social` contract [`Post`].
/// Instagram posts have a `media[]` array or a single `media` object; the
/// caption lives at the post-root `title` field (primary) or as a fallback
/// in `media[0].title` (per-image alt text).
/// `None` when there is no usable timestamp.
fn post_from_value(raw: &Value, kind: &str) -> Option<Post> {
    let obj = raw.as_object()?;

    // Timestamp: real Meta Accounts Center exports use `creation_timestamp`
    // at the post root.  Fall back to the legacy `timestamp` alias so that
    // older / third-party-generated exports still parse.
    let unix = obj
        .get("creation_timestamp")
        .and_then(Value::as_i64)
        .or_else(|| obj.get("timestamp").and_then(Value::as_i64))?;
    let ts = DateTime::from_timestamp(unix, 0)?.with_timezone(&Local).to_rfc3339();

    // Normalise `media` to a Vec regardless of whether the export delivers a
    // bare object (single-media posts) or an array (multi-media / carousel).
    let media_vec: Vec<&Value> = match obj.get("media") {
        Some(Value::Array(arr)) => arr.iter().collect(),
        Some(obj_val @ Value::Object(_)) => vec![obj_val],
        _ => Vec::new(),
    };

    // Caption: post-root `title` is the canonical caption field.
    // Fall back to `media[0].title` (per-image alt text) when absent.
    let caption = obj
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| {
            media_vec
                .first()
                .and_then(|m| m.get("title").and_then(Value::as_str))
        })
        .unwrap_or("")
        .to_string();

    // Media metadata: each `media[]` entry → a [`Media`] row (never copied).
    let mut media: Vec<Media> = Vec::new();
    for m in &media_vec {
        let uri = m.get("uri").and_then(Value::as_str).unwrap_or("").to_string();
        let alt = m.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        let media_type = infer_media_type(&uri);
        if !uri.is_empty() {
            media.push(Media { r#type: media_type, url: uri, alt });
        }
    }

    // guid: sha256 over (unix_ts | caption | primary_uri) — no stable id.
    let primary_uri = media.first().map(|m| m.url.as_str()).unwrap_or("");
    let guid = post_guid(unix, &caption, primary_uri);

    let mut post = Post::new(SOURCE, guid, ts);
    post.kind = kind.to_string();
    if !caption.is_empty() {
        post.text = caption;
    }
    if !media.is_empty() {
        post.media = media;
    }

    // Overflow: any top-level key not already mapped → extra.
    for (k, v) in obj {
        if matches!(k.as_str(), "creation_timestamp" | "timestamp" | "media" | "title") {
            continue;
        }
        post.extra.entry(k.clone()).or_insert_with(|| v.clone());
    }

    Some(post)
}

fn post_guid(unix: i64, caption: &str, uri: &str) -> String {
    let mut h = Sha256::new();
    let mut feed = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    feed(unix.to_string().as_bytes());
    feed(caption.as_bytes());
    feed(uri.as_bytes());
    format!("{:x}", h.finalize())
}

fn infer_media_type(uri: &str) -> String {
    let u = uri.to_ascii_lowercase();
    if u.ends_with(".mp4") || u.ends_with(".mov") || u.ends_with(".webm") {
        "video".into()
    } else if u.ends_with(".jpg") || u.ends_with(".jpeg") || u.ends_with(".png")
        || u.ends_with(".heic") || u.ends_with(".webp")
    {
        "image".into()
    } else if u.ends_with(".gif") {
        "gif".into()
    } else {
        "media".into()
    }
}

// ---------------------------------------------------------------------------
// Threads → social/threads/ contract.

/// Parse `threads_and_replies.json` → social [`Post`] rows for the `threads`
/// provider. The schema mirrors Instagram posts (`creation_timestamp` at the
/// root, post-root `title` for the caption) but may also carry a `post` field
/// directly on each object as an alternative caption source.
fn parse_threads(
    value: &Value,
    seen: &mut HashSet<String>,
    out: &mut Vec<Post>,
    stats: &mut Stats,
) {
    for item in posts_array(value) {
        let Some(obj) = item.as_object() else { continue };
        // Real Meta exports use `creation_timestamp`; fall back to `timestamp`.
        let Some(unix) = obj
            .get("creation_timestamp")
            .and_then(Value::as_i64)
            .or_else(|| obj.get("timestamp").and_then(Value::as_i64))
        else {
            continue;
        };
        let Some(dt) = DateTime::from_timestamp(unix, 0)
            .map(|dt| dt.with_timezone(&Local).to_rfc3339())
        else {
            continue;
        };

        // Normalise `media` to Vec regardless of array vs single-object.
        let media_vec: Vec<&Value> = match obj.get("media") {
            Some(Value::Array(arr)) => arr.iter().collect(),
            Some(obj_val @ Value::Object(_)) => vec![obj_val],
            _ => Vec::new(),
        };

        // Caption: post-root `title` → `post` field → `media[0].title`.
        let caption = obj
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| obj.get("post").and_then(Value::as_str))
            .or_else(|| {
                media_vec
                    .first()
                    .and_then(|m| m.get("title").and_then(Value::as_str))
            })
            .unwrap_or("")
            .to_string();

        let primary_uri = media_vec
            .first()
            .and_then(|m| m.get("uri").and_then(Value::as_str))
            .unwrap_or("");

        let guid = post_guid(unix, &caption, primary_uri);
        if !seen.insert(guid.clone()) {
            stats.duplicates += 1;
            continue;
        }

        let mut post = Post::new(THREADS_SOURCE, guid, dt);
        post.kind = "post".into();
        if !caption.is_empty() {
            post.text = caption;
        }
        for (k, v) in obj {
            if matches!(
                k.as_str(),
                "creation_timestamp" | "timestamp" | "media" | "title" | "post"
            ) {
                continue;
            }
            post.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
        out.push(post);
        stats.threads_posts += 1;
    }
}

// ---------------------------------------------------------------------------
// DMs → correspondence contract.

/// Parse one DM thread file into [`Message`] rows.
///
/// Instagram's inbox JSON (same schema as Messenger):
/// ```json
/// {
///   "participants": [{"name": "Alice"}, {"name": "Bob"}],
///   "messages": [
///     {
///       "sender_name": "Alice",
///       "timestamp_ms": 1718000000000,
///       "content": "hello",
///       "reactions": [{"reaction": "😍", "actor": "Bob"}],
///       "photos": [{"uri": "messages/inbox/.../photo1.jpg", "creation_timestamp": 1718000001}],
///       "videos": [{"uri": "messages/inbox/.../video1.mp4", "creation_timestamp": 1718000002}],
///       "share": {"link": "https://example.com", "share_text": "check this"}
///     }
///   ],
///   "title": "Alice and Bob"
/// }
/// ```
fn parse_dm_thread(
    value: &Value,
    thread_key: &str,
    owner_name: Option<&str>,
    seen: &mut HashSet<String>,
    out: &mut Vec<Message>,
    stats: &mut Stats,
) {
    let obj = match value.as_object() {
        Some(o) => o,
        None => return,
    };

    // Thread display name ("title" or participants joined).
    let chat_name = obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let msgs = match obj.get("messages").and_then(Value::as_array) {
        Some(arr) => arr,
        None => return,
    };

    for raw_msg in msgs {
        let Some(m) = raw_msg.as_object() else { continue };
        let Some(ts_ms) = m.get("timestamp_ms").and_then(Value::as_i64) else { continue };
        let ts_secs = ts_ms / 1000;
        let ts_nanos = ((ts_ms % 1000) * 1_000_000) as u32;
        let Some(dt) = DateTime::from_timestamp(ts_secs, ts_nanos) else { continue };
        let ts = dt.with_timezone(&Local).to_rfc3339();

        let sender_name = m.get("sender_name").and_then(Value::as_str).unwrap_or("").to_string();
        let content = m.get("content").and_then(Value::as_str).unwrap_or("").to_string();

        // Reactions ship as separate Message rows (`kind:"reaction"`).
        if let Some(reactions) = m.get("reactions").and_then(Value::as_array) {
            for (i, rx) in reactions.iter().enumerate() {
                let actor = rx.get("actor").and_then(Value::as_str).unwrap_or("").to_string();
                let emoji = rx.get("reaction").and_then(Value::as_str).unwrap_or("").to_string();
                let guid = reaction_guid(thread_key, &sender_name, ts_ms, &emoji, i);
                if !seen.insert(guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                let mut rxm = Message::new(SOURCE, ts.clone());
                rxm.chat = thread_key.to_string();
                rxm.chat_name = chat_name.clone();
                rxm.sender_name = actor.clone();
                rxm.sender = actor;
                rxm.from_me = owner_name.is_some_and(|o| rxm.sender_name == o);
                rxm.kind = "reaction".into();
                rxm.reaction = emoji;
                rxm.reply_to = msg_guid(thread_key, &sender_name, ts_ms, &content);
                rxm.service = "Instagram".into();
                rxm.guid = guid;
                out.push(rxm);
                stats.messages += 1;
            }
        }

        // The message itself.
        let guid = msg_guid(thread_key, &sender_name, ts_ms, &content);
        if !seen.insert(guid.clone()) {
            stats.duplicates += 1;
            continue;
        }

        let from_me = owner_name.is_some_and(|o| sender_name == o);
        let mut msg = Message::new(SOURCE, ts);
        msg.chat = thread_key.to_string();
        msg.chat_name = chat_name.clone();
        msg.sender_name = sender_name.clone();
        // Canonical sender address = display name (no handle in the export).
        msg.sender = if from_me { String::new() } else { sender_name };
        msg.from_me = from_me;
        msg.text = content.clone();
        msg.service = "Instagram".into();
        msg.guid = guid;

        // Media attachments — metadata only, never copy the files.
        let mut attachments = Vec::new();
        for (key, mime) in [("photos", "image/jpeg"), ("videos", "video/mp4"), ("audio_files", "audio/mp4")] {
            if let Some(arr) = m.get(key).and_then(Value::as_array) {
                for item in arr {
                    let name = item.get("uri").and_then(Value::as_str).unwrap_or("").to_string();
                    attachments.push(crate::correspondence::AttachmentMeta {
                        name,
                        mime: mime.to_string(),
                        bytes: 0,
                    });
                }
            }
        }
        if let Some(share) = m.get("share") {
            // Shared link — record as a text prefix when there's no other content.
            if msg.text.is_empty() {
                let link = share.get("link").and_then(Value::as_str).unwrap_or("");
                let share_text = share.get("share_text").and_then(Value::as_str).unwrap_or("");
                if !link.is_empty() {
                    msg.text = if share_text.is_empty() {
                        link.to_string()
                    } else {
                        format!("{share_text} {link}")
                    };
                }
            }
        }
        if !attachments.is_empty() {
            msg.attachments = attachments;
        }

        out.push(msg);
        stats.messages += 1;
    }
}

/// Deterministic guid for a DM message — no stable id in the export.
fn msg_guid(thread: &str, sender: &str, ts_ms: i64, content: &str) -> String {
    let mut h = Sha256::new();
    for part in [thread, sender, &ts_ms.to_string(), content] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn reaction_guid(thread: &str, sender: &str, ts_ms: i64, emoji: &str, idx: usize) -> String {
    let mut h = Sha256::new();
    for part in [thread, sender, &ts_ms.to_string(), emoji, &idx.to_string()] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Raw sections.

/// Unwrap the common DYI section shape: a one-key object whose value is an
/// array, or a bare top-level array, else the whole object as one item.
fn section_items(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    if let Some(obj) = value.as_object() {
        let array_vals: Vec<&Value> = obj.values().filter(|v| v.is_array()).collect();
        if array_vals.len() == 1 {
            if let Some(arr) = array_vals[0].as_array() {
                return arr.clone();
            }
        }
        return vec![value.clone()];
    }
    Vec::new()
}

fn content_hash(item: &Value) -> String {
    let mut h = Sha256::new();
    h.update(item.to_string().as_bytes());
    format!("{:x}", h.finalize())
}

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
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-instagram-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn params(owner: Option<&str>) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("acknowledge".into(), "yes".into());
        if let Some(o) = owner {
            m.insert("owner_name".into(), o.into());
        }
        m
    }

    fn run(v: &Vault, path: &Path, owner: Option<&str>) -> ImportOutcome {
        (IMPORT.run)(v, path, &params(owner), &mut |_| {}).unwrap()
    }

    /// Build a synthetic Instagram-style export ZIP for testing.
    ///
    /// Layout:
    /// - `content/posts_1.json`   — two posts (one with caption + image, one mojibake)
    /// - `content/stories.json`   — one story
    /// - `messages/inbox/alice_abc123/message_1.json` — a DM thread (two messages + reaction)
    /// - `threads_and_replies/threads_and_replies.json` — one Threads post
    /// - `connections/followers_and_following/followers_1.json` — raw section
    /// - `media/posts/201906/photo1.jpg` — binary media (must be ignored)
    fn export_zip(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-ig-{}-{label}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Posts: real Meta Accounts Center shape — `creation_timestamp` at the
        // post root, caption in post-root `title`, `media[].title` is per-image
        // alt text (may differ from the caption).
        // The second post has a mojibake caption: "caf\u{c3}\u{a9}" = café.
        z.start_file("content/posts_1.json", opts).unwrap();
        // "cafÃ©" is Meta mojibake for "café": é → UTF-8 C3 A9 → Latin-1 Ã©.
        // concat! lets us embed \u{} escapes in a non-raw string literal.
        let posts_json = concat!(
            r#"[{"creation_timestamp":1718000000,"title":"Sunset at the beach","media":[{"uri":"media/posts/201906/photo1.jpg","creation_timestamp":1718000000}]},"#,
            r#"{"creation_timestamp":1718000100,"title":"caf"#,
            "\u{c3}\u{a9}",  // é as Meta mojibake (Latin-1 Ã©)
            r#" morning","media":[{"uri":"media/posts/201906/photo2.jpg","creation_timestamp":1718000100}]}]"#,
        );
        z.write_all(posts_json.as_bytes()).unwrap();

        // Stories: same real shape.
        z.start_file("content/stories.json", opts).unwrap();
        z.write_all(br#"[
          {"creation_timestamp":1718001000,"title":"Morning run","media":[{"uri":"media/stories/201906/story1.jpg","creation_timestamp":1718001000}]}
        ]"#).unwrap();

        // DMs — Messenger schema. The reaction emoji 😍 (U+1F60D) is stored as
        // Meta mojibake in the export: UTF-8 bytes F0 9F 98 8D mis-read as Latin-1
        // → 4 chars \u{f0}\u{9f}\u{98}\u{8d}. Build with a Rust string so we can
        // use \u{} escapes (raw byte strings reject non-ASCII).
        let dm_json = concat!(
            r#"{"participants":[{"name":"Alice Smith"},{"name":"Bob Jones"}],"title":"Alice Smith","messages":["#,
            r#"{"sender_name":"Alice Smith","timestamp_ms":1718100000000,"content":"Hey Bob!"},"#,
            r#"{"sender_name":"Bob Jones","timestamp_ms":1718100060000,"content":"Hi Alice!","reactions":[{"reaction":""#,
            "\u{f0}\u{9f}\u{98}\u{8d}",  // 😍 as Latin-1 mojibake
            r#"","actor":"Alice Smith"}]}"#,
            r#"]}"#,
        );
        z.start_file("messages/inbox/alice_abc123/message_1.json", opts).unwrap();
        z.write_all(dm_json.as_bytes()).unwrap();

        // Threads: same real Meta shape — `creation_timestamp` at root, caption
        // in post-root `title`.
        z.start_file("threads_and_replies/threads_and_replies.json", opts).unwrap();
        z.write_all(br#"[
          {"creation_timestamp":1718200000,"title":"My first Threads post","media":[{"uri":"threads/media/photo.jpg","creation_timestamp":1718200000}]}
        ]"#).unwrap();

        // Followers — raw section.
        z.start_file("connections/followers_and_following/followers_1.json", opts).unwrap();
        z.write_all(br#"{"relationships_followers":[{"string_list_data":[{"value":"alice_insta","timestamp":1700000000}]}]}"#).unwrap();

        // Binary media file — must be silently ignored.
        z.start_file("media/posts/201906/photo1.jpg", opts).unwrap();
        z.write_all(b"\xff\xd8\xff\xe0not-a-real-jpeg").unwrap();

        z.finish().unwrap();
        path
    }

    #[test]
    fn imports_posts_to_social_contract() {
        let v = temp_vault("posts");
        let zip = export_zip("posts");
        let out = run(&v, &zip, Some("Bob Jones"));

        assert_eq!(out.counts.get("posts"), Some(&3), "2 posts + 1 story: {}", out.headline);

        // Posts land in social/instagram/YYYY-MM.jsonl; 1718000000 = 2024-06.
        let raw = fs::read_to_string(v.root().join("social/instagram/2024-06.jsonl")).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(rows.len(), 3, "2 posts + 1 story in 2024-06");

        let beach = rows.iter().find(|p| p.text == "Sunset at the beach").unwrap();
        assert_eq!(beach.kind, "post");
        assert_eq!(beach.source, "instagram");
        assert_eq!(beach.media[0].url, "media/posts/201906/photo1.jpg");
        assert_eq!(beach.media[0].r#type, "image");
        assert_eq!(beach.guid.len(), 64);

        // Mojibake decoded: "caf\u{c3}\u{a9} morning" → "café morning".
        let moji = rows.iter().find(|p| p.text == "café morning").unwrap();
        assert!(!moji.text.contains('\u{c3}'), "mojibake must be fixed");

        // Story has kind "story".
        let story = rows.iter().find(|p| p.kind == "story").unwrap();
        assert_eq!(story.text, "Morning run");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn imports_dms_to_correspondence() {
        let v = temp_vault("dms");
        let zip = export_zip("dms");
        let out = run(&v, &zip, Some("Bob Jones"));

        // 2 messages + 1 reaction = 3 correspondence rows.
        assert_eq!(out.counts.get("messages"), Some(&3), "2 messages + 1 reaction: {}", out.headline);

        let month = "2024-06"; // 1718100000 secs / 1000 = 2024-06
        let path = v.root().join(format!("correspondence/instagram/{month}.jsonl"));
        assert!(path.exists(), "correspondence file exists");
        let body = fs::read_to_string(&path).unwrap();
        let msgs: Vec<Message> =
            body.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        let alice_msg = msgs.iter().find(|m| m.sender_name == "Alice Smith" && m.kind == "message").unwrap();
        assert_eq!(alice_msg.text, "Hey Bob!");
        assert_eq!(alice_msg.from_me, false, "Alice is not the owner");
        assert_eq!(alice_msg.service, "Instagram");
        assert_eq!(alice_msg.chat, "alice_abc123", "thread folder used as chat key");

        let bob_msg = msgs.iter().find(|m| m.sender_name == "Bob Jones" && m.kind == "message").unwrap();
        assert_eq!(bob_msg.from_me, true, "Bob Jones is the owner");
        assert!(bob_msg.sender.is_empty(), "sender empty when from_me");

        // The reaction on Bob's message: emoji was mojibake for 😍.
        let reaction = msgs.iter().find(|m| m.kind == "reaction").unwrap();
        assert_eq!(reaction.reaction, "😍", "reaction emoji mojibake decoded");
        assert_eq!(reaction.sender_name, "Alice Smith");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn imports_threads_posts() {
        let v = temp_vault("threads");
        let zip = export_zip("threads");
        let out = run(&v, &zip, None);

        assert_eq!(out.counts.get("threads_posts"), Some(&1), "one Threads post: {}", out.headline);
        let path = v.root().join("social/threads/2024-06.jsonl");
        assert!(path.exists());
        let body = fs::read_to_string(&path).unwrap();
        let posts: Vec<Post> = body.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].source, "threads");
        assert_eq!(posts[0].text, "My first Threads post");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn writes_raw_sections_full_fidelity() {
        let v = temp_vault("raw");
        let zip = export_zip("raw");
        let out = run(&v, &zip, None);

        // Raw: posts raw layer + followers section.
        assert!(out.counts.get("raw").unwrap() >= &1, "raw items: {}", out.headline);

        // Followers → raw/followers.jsonl (version suffix stripped from "followers_1").
        let path = v.root().join("social/instagram/raw/followers.jsonl");
        assert!(path.exists(), "followers raw file");
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("alice_insta"), "raw content preserved");

        // Posts raw layer — full-fidelity copies under raw/posts.jsonl.
        let raw_posts = v.root().join("social/instagram/raw/posts.jsonl");
        assert!(raw_posts.exists(), "raw posts file");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reimport_deduplicates() {
        let v = temp_vault("dedup");
        let zip = export_zip("dedup");

        let first = run(&v, &zip, Some("Bob Jones"));
        let posts_first = first.counts.get("posts").copied().unwrap_or(0);
        let msgs_first = first.counts.get("messages").copied().unwrap_or(0);

        let second = run(&v, &zip, Some("Bob Jones"));
        assert_eq!(second.counts.get("posts"), Some(&0), "all posts deduped on re-import");
        assert_eq!(second.counts.get("messages"), Some(&0), "all messages deduped on re-import");
        let dups = second.counts.get("duplicates").copied().unwrap_or(0);
        assert!(dups >= posts_first + msgs_first, "at least original items skipped as duplicates");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn ignores_binary_media_files() {
        let v = temp_vault("media");
        let zip = export_zip("media");
        // Should not error; non-JSON files just silently skip.
        let out = run(&v, &zip, None);
        // The outcome must not reference any .jpg data.
        assert!(!out.headline.to_lowercase().contains("error"), "no error in headline");
        let _ = fs::remove_file(zip);
    }

    #[test]
    fn section_name_strips_version_suffix() {
        assert_eq!(section_name("connections/followers_and_following/followers_1.json"), "followers");
        assert_eq!(section_name("ads_information/ads_viewed.json"), "ads_viewed");
        assert_eq!(section_name("content/posts_1.json"), "posts");
        assert_eq!(section_name("content/stories.json"), "stories");
    }

    #[test]
    fn thread_title_extracted_from_path() {
        assert_eq!(
            thread_title_from_path("messages/inbox/alice_abc123/message_1.json"),
            "alice_abc123"
        );
        assert_eq!(
            thread_title_from_path("messages/archived_threads/group_xyz/message_1.json"),
            "group_xyz"
        );
    }

    #[test]
    fn post_kind_inferred_from_path() {
        assert_eq!(post_kind_from_path("content/posts_1.json"), "post");
        assert_eq!(post_kind_from_path("content/stories.json"), "story");
        assert_eq!(post_kind_from_path("content/reels.json"), "reel");
        assert_eq!(post_kind_from_path("content/reels_1.json"), "reel");
    }

    #[test]
    fn is_dm_file_routing() {
        assert!(is_dm_file("messages/inbox/alice_abc123/message_1.json"));
        assert!(is_dm_file("messages/archived_threads/group_xyz/message_2.json"));
        assert!(!is_dm_file("content/posts_1.json"));
        assert!(!is_dm_file("connections/followers_and_following/followers_1.json"));
    }
}
