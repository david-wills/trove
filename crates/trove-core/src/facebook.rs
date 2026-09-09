//! Facebook — a file-import source ([`Behavior::Import`]): Facebook has no
//! viable personal API (the Graph API is gated behind app review for consumer
//! data), so the only sanctioned path is the official **Download Your
//! Information** (DYI) JSON export ZIP (Settings → Your Facebook Information →
//! Download Your Information → JSON format). No auth, no network, no TCC —
//! standalone-clean.
//!
//! The DYI ZIP carries intimate categories (posts, friends, reactions, search
//! history, events, groups, ad interactions, and — handled elsewhere —
//! Messenger threads), so this integration is **default-off, opt-in** with a
//! privacy acknowledgement at import time.
//!
//! ## What lands where
//!
//! - **Posts** → BOTH layers. The normalized `social` contract row
//!   (`social/facebook/YYYY-MM.jsonl`, `kind:"post"`, see [`crate::social`]) is
//!   the curated view; the export carries no stable post id, so [`Post::guid`]
//!   is a length-prefixed `sha256` over (unix timestamp, text, FB title) —
//!   injective and stable across re-imports. The **full decoded post object**
//!   is ALSO written to the raw layer (`social/facebook/raw/posts.jsonl`), so
//!   fields the contract mapping doesn't carry (`data[]` siblings like
//!   `update_timestamp`/`backdated_timestamp`, `external_context.name`/`.source`)
//!   are never lost — posts are just one more raw section for the raw write.
//! - **Everything else** (friends, reactions, search history, events, groups,
//!   ads, and any other section) → the raw layer
//!   `social/facebook/raw/<section>.jsonl`: each decoded item full-fidelity,
//!   one JSON object per line, deduped by a content hash. **Unknown sections
//!   are kept**, not dropped — the raw layer is generic over the export's
//!   shape.
//! - **Messenger** (`messages/inbox/…`) is **detected and skipped** here — it
//!   belongs to the `facebook-messenger` integration (#72, not yet built). A
//!   log note records the deferral; nothing is parsed and no error is raised.
//!
//! ## Meta mojibake
//!
//! DYI strings are UTF-8 bytes mis-stored as Latin-1. Every string value parsed
//! out of the export — post text, titles, tag names, attachment descriptions,
//! friend names, search terms — is repaired through
//! [`crate::meta_encoding::fix_meta_encoding`] (recursively, via
//! [`crate::meta_encoding::fix_value`]). Pure ASCII and already-correct emoji
//! pass through unchanged.
//!
//! ## Media
//!
//! Attached media is **metadata only** — the in-ZIP relative path / external
//! URL and the author's description ride in [`Post::media`]; the files
//! themselves are never copied into the vault.
//!
//! ## Dedupe
//!
//! Posts dedupe by [`Post::guid`]; raw rows by a per-section content hash.
//! Re-importing a newer (or the same) export never duplicates — it upserts new
//! items and skips the rest.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};
use serde_json::{json, Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::meta_encoding::fix_value;
use crate::registry::{Behavior, ImportOutcome, ImportParam, ImportSpec, IntegrationDef};
use crate::social::{Media, Post};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "facebook";
const DIR: &str = "social/facebook";
const RAW_DIR: &str = "social/facebook/raw";
/// Raw-section name for the full decoded post objects (kept losslessly
/// alongside the normalized contract rows).
const POSTS_SECTION: &str = "posts";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "facebook",
        name: "Facebook",
        kind: IntegrationKind::Import,
        // 🔒 The DYI ZIP carries intimate categories — opt-in only.
        default_on: false,
        description: "Import your Facebook history from the official Download Your Information \
                      (DYI) JSON export: your posts join the unified social stream, and friends, \
                      reactions, search history, events, groups, and ad interactions are kept \
                      full-fidelity. Re-runnable; newer exports never duplicate. Messenger \
                      conversations in the same export are handled by the Facebook Messenger \
                      integration.",
        domain: "social",
        vault_path: "social/facebook/",
        toggleable: false,
        setup: &[
            "Facebook → Settings → Your Facebook Information → Download Your Information → \
             choose JSON format, select your categories and date range, and request the \
             download. A ZIP arrives by notification/email (usually within hours).",
            "Drop the ZIP here as-is. This export carries intimate categories (posts, friends, \
             reactions, search history, and more); import it only if you intend to store that \
             data in your private vault. Media files are never copied — only their metadata.",
        ],
        caveats: "The export carries no stable post id, so posts are deduped by a content hash \
                  (timestamp + text) — editing a post's text after exporting will look like a \
                  new post on the next import. Attachments are metadata only (path/URL + \
                  description), never downloaded. Messenger threads in the same ZIP are skipped \
                  here — the Facebook Messenger integration owns them.",
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
    params: &[ImportParam {
        // 🔒 Opt-in acknowledgement — the hub renders this as a required field
        // so the user actively confirms before the intimate export is read.
        key: "acknowledge",
        label: "Privacy acknowledgement",
        placeholder: "Type 'yes' to confirm you want this intimate export stored in your vault",
        required: true,
    }],
    run: run_import,
};

#[derive(Default)]
struct Stats {
    posts: u64,
    raw: u64,
    duplicates: u64,
    sections: HashSet<String>,
    messenger_detected: bool,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Posts already stored, for re-runnable imports (dedupe by guid).
    let post_stream = vault.stream(DIR, Partition::Month);
    let mut seen_posts: HashSet<String> = HashSet::new();
    for key in post_stream.partitions()? {
        for p in post_stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen_posts.insert(p.guid);
            }
        }
    }

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a Facebook DYI export ZIP?", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    let mut stats = Stats::default();
    let mut posts: Vec<Post> = Vec::new();
    // Raw section rows, grouped by section file, with a per-section seen-set
    // (loaded lazily the first time a section is touched) so re-imports dedupe.
    let mut raw_by_section: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut raw_seen: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    for name in &names {
        // Messenger: detect and skip — owned by facebook-messenger (#72).
        if is_messenger(name) {
            stats.messenger_detected = true;
            continue;
        }

        let lower = name.to_ascii_lowercase();
        if !lower.ends_with(".json") {
            continue; // media files, HTML, etc. — never copied.
        }

        let mut body = String::new();
        if read_entry(&mut zip, name, &mut body).is_err() {
            continue;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&body) else {
            continue; // not JSON we can read; skip leniently.
        };
        // Repair Meta mojibake across every string in this section.
        fix_value(&mut value);

        if is_posts_file(name) {
            // Posts route to BOTH layers: the normalized `social` contract row
            // (the curated view) AND the raw layer, where the full decoded post
            // object is preserved losslessly — so `data[]` siblings
            // (update/backdated timestamps) and `external_context.name`/`.source`
            // that the contract mapping doesn't carry are never dropped. Posts
            // are treated as one more raw section ("posts") for the raw write.
            let seen_raw = raw_seen
                .entry(POSTS_SECTION.to_string())
                .or_insert_with(|| load_raw_section_guids(vault, POSTS_SECTION));
            let raw_bucket = raw_by_section.entry(POSTS_SECTION.to_string()).or_default();
            for raw_post in posts_array(&value) {
                // Raw, full-fidelity copy (mojibake already fixed above).
                let raw_guid = content_hash(raw_post);
                if seen_raw.insert(raw_guid.clone()) {
                    raw_bucket.push(
                        json!({"section": POSTS_SECTION, "guid": raw_guid, "raw": raw_post.clone()}),
                    );
                    stats.raw += 1;
                    stats.sections.insert(POSTS_SECTION.to_string());
                }
                // Normalized contract row.
                let Some(post) = post_from_value(raw_post) else { continue };
                if !seen_posts.insert(post.guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                posts.push(post);
                stats.posts += 1;
            }
        } else {
            // Any other JSON section → the raw layer, full fidelity.
            let section = section_name(name);
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

    // Write posts to the contract stream, partitioned by month of ts.
    post_stream.append(&posts, |p| &p.ts)?;
    // Write raw rows, one append-only file per section (no date partition —
    // these are reference sections, keyed by content hash).
    for (section, rows) in &raw_by_section {
        if rows.is_empty() {
            continue;
        }
        append_raw_section(vault, section, rows)?;
    }

    progress(ImportProgress { records: stats.posts + stats.raw, percent: 100.0 });

    let mut headline = format!(
        "{} posts imported, {} raw items across {} sections, {} duplicates skipped",
        stats.posts,
        stats.raw,
        stats.sections.len(),
        stats.duplicates
    );
    if stats.messenger_detected {
        // The log note: Messenger conversations detected — deferred to #72.
        headline.push_str(
            " (Messenger conversations detected — deferred to the Facebook Messenger integration #72, not imported)",
        );
    }

    Ok(ImportOutcome {
        headline,
        counts: [
            ("posts", stats.posts),
            ("raw", stats.raw),
            ("sections", stats.sections.len() as u64),
            ("duplicates", stats.duplicates),
            ("messenger_skipped", stats.messenger_detected as u64),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// ZIP entry routing.

/// A Messenger conversation file: `…/messages/inbox/<thread>/message_N.json`
/// (also `messages/archived_threads/`, `messages/filtered_threads/`). We only
/// need to *detect* it; the parse is deferred to facebook-messenger (#72).
fn is_messenger(name: &str) -> bool {
    let n = name.replace('\\', "/");
    n.contains("messages/inbox/")
        || n.contains("messages/archived_threads/")
        || n.contains("messages/filtered_threads/")
        || n.contains("messages/e2ee_cutover/")
}

/// `…/posts/your_posts_1.json` (and numbered siblings). DYI nests posts under
/// `your_activity_across_facebook/posts/` in newer exports and `posts/` in
/// older ones; match the filename to be layout-agnostic.
fn is_posts_file(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    file.starts_with("your_posts") && file.ends_with(".json")
}

/// A stable, human-meaningful section label from a DYI entry path: the file
/// stem, lowercased, with a trailing `_v2`/`_N` version/shard suffix stripped
/// so `friends_v2.json` and `reactions_1.json` collapse to `friends` /
/// `reactions`. Keeps unknown sections (generic).
fn section_name(name: &str) -> String {
    let stem = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .strip_suffix(".json")
        .or_else(|| name.rsplit('/').next())
        .unwrap_or(name)
        .to_ascii_lowercase();
    // Drop a trailing `_v<digits>` or `_<digits>` shard/version suffix.
    let trimmed = strip_version_suffix(&stem);
    trimmed.to_string()
}

/// Strip a trailing `_v2` / `_v10` / `_3` style suffix from a section stem.
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
// Posts → social contract.

/// The posts array out of a `your_posts_*.json` value, handling BOTH shapes:
/// the new bare array `[ … ]`, and the old `{"status_updates": [ … ]}`
/// wrapper. Anything else yields nothing.
fn posts_array(value: &Value) -> Vec<&Value> {
    if let Some(arr) = value.as_array() {
        return arr.iter().collect();
    }
    if let Some(arr) = value.get("status_updates").and_then(Value::as_array) {
        return arr.iter().collect();
    }
    Vec::new()
}

/// One DYI post object → a `social` contract [`Post`]. `None` when there is no
/// usable timestamp (the partition key).
fn post_from_value(raw: &Value) -> Option<Post> {
    let obj = raw.as_object()?;
    let unix = obj.get("timestamp").and_then(Value::as_i64)?;
    let ts = DateTime::from_timestamp(unix, 0)?.with_timezone(&Local).to_rfc3339();

    // `data` MAY be absent (an image-only post) or empty → no text.
    let text = obj
        .get("data")
        .and_then(Value::as_array)
        .and_then(|arr| arr.iter().find_map(|d| d.get("post").and_then(Value::as_str)))
        .unwrap_or("")
        .to_string();

    // FB's synthesized title ("David posted a photo.") is NOT a user title —
    // it rides in extra.fb_title, never in `Post::title`.
    let fb_title = obj.get("title").and_then(Value::as_str).unwrap_or("").to_string();

    // guid = sha256(unix_ts | text | fb_title) hex — no stable id in the export.
    let guid = post_guid(unix, &text, &fb_title);

    let mut post = Post::new(SOURCE, guid, ts);
    post.kind = "post".into();
    if !text.is_empty() {
        post.text = text;
    }

    // Media + a link (external_context) from attachments[].data[].
    let mut media: Vec<Media> = Vec::new();
    let mut url = String::new();
    if let Some(atts) = obj.get("attachments").and_then(Value::as_array) {
        for att in atts {
            let Some(data) = att.get("data").and_then(Value::as_array) else { continue };
            for d in data {
                if let Some(m) = d.get("media").and_then(Value::as_object) {
                    media.push(Media {
                        r#type: media_type(m),
                        url: m.get("uri").and_then(Value::as_str).unwrap_or("").to_string(),
                        alt: m
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    });
                } else if let Some(ext) = d.get("external_context").and_then(Value::as_object) {
                    // A link post: prefer it as the canonical `url`; if there's
                    // already one, record additional links as `link` media.
                    let link = ext.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                    if link.is_empty() {
                        continue;
                    }
                    if url.is_empty() {
                        url = link;
                    } else {
                        media.push(Media { r#type: "link".into(), url: link, alt: String::new() });
                    }
                } else if let Some(place) = d.get("place").and_then(Value::as_object) {
                    // A place check-in → extra (not media, not a link).
                    post.extra
                        .insert("place".into(), Value::Object(place.clone()));
                }
            }
        }
    }
    if !url.is_empty() {
        post.url = url;
    }
    if !media.is_empty() {
        post.media = media;
    }

    // tags from tags[].name.
    let tags: Vec<String> = obj
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !tags.is_empty() {
        post.tags = tags;
    }

    // FB synthesized title → extra.fb_title (never Post::title).
    if !fb_title.is_empty() {
        post.extra.insert("fb_title".into(), Value::String(fb_title));
    }
    // Overflow: every other top-level key we didn't map → extra, full fidelity.
    for (k, v) in obj {
        if matches!(k.as_str(), "timestamp" | "data" | "attachments" | "tags" | "title") {
            continue;
        }
        post.extra.entry(k.clone()).or_insert_with(|| v.clone());
    }

    Some(post)
}

/// A length-prefixed sha256 over (unix_ts, text, fb_title) as lowercase hex —
/// the dedupe key for posts (the export carries no stable id). Each component
/// is fed as its byte length (u64 LE) followed by its bytes, so the boundaries
/// are unambiguous: a literal `|` (or any byte) in the user text can never
/// shift one component into another, i.e. `("a|b","c")` and `("a","b|c")` hash
/// differently. Hashed over the POST-mojibake-fix text so re-imports still
/// match the same post.
fn post_guid(unix: i64, text: &str, fb_title: &str) -> String {
    let mut h = Sha256::new();
    let mut feed = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    feed(unix.to_string().as_bytes());
    feed(text.as_bytes());
    feed(fb_title.as_bytes());
    format!("{:x}", h.finalize())
}

/// Best-effort media type for an attachment's `media` object: an explicit
/// `media_type`/`type`, else inferred from the uri extension, else "media".
fn media_type(m: &Map<String, Value>) -> String {
    if let Some(t) = m.get("media_type").or_else(|| m.get("type")).and_then(Value::as_str) {
        if !t.is_empty() {
            return t.to_ascii_lowercase();
        }
    }
    let uri = m.get("uri").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    if uri.ends_with(".mp4") || uri.ends_with(".mov") || uri.ends_with(".webm") {
        "video".into()
    } else if uri.ends_with(".gif") {
        "gif".into()
    } else if uri.ends_with(".jpg")
        || uri.ends_with(".jpeg")
        || uri.ends_with(".png")
        || uri.ends_with(".heic")
        || uri.ends_with(".webp")
    {
        "image".into()
    } else {
        "media".into()
    }
}

// ---------------------------------------------------------------------------
// Raw sections.

/// The items of a raw DYI section value. DYI sections are usually a single-key
/// object whose value is an array (`{"friends_v2": [ … ]}`,
/// `{"reactions_v2": [ … ]}`); also handle a bare top-level array, and fall
/// back to the whole object as one item.
fn section_items(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    if let Some(obj) = value.as_object() {
        // The common DYI shape: one wrapper key → an array.
        let array_vals: Vec<&Value> = obj.values().filter(|v| v.is_array()).collect();
        if array_vals.len() == 1 {
            if let Some(arr) = array_vals[0].as_array() {
                return arr.clone();
            }
        }
        // Otherwise keep the whole object as a single raw item (don't drop it).
        return vec![value.clone()];
    }
    Vec::new()
}

/// A stable content hash of a raw item, for re-import dedupe. Canonicalised by
/// `serde_json` (BTreeMap key order is deterministic for objects).
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

/// Append raw rows to `social/facebook/raw/<section>.jsonl` (one object per
/// line, newline-terminated). Not date-partitioned — reference sections.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-facebook-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn ack() -> BTreeMap<String, String> {
        [("acknowledge".to_string(), "yes".to_string())].into()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &ack(), &mut |_| {}).unwrap()
    }

    /// A synthetic DYI export slice exercising every shape the importer cares
    /// about. `posts_body` is written under the (newer) nested posts path.
    fn export_zip(name: &str, posts_body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("trove-fbdyi-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        z.start_file("your_activity_across_facebook/posts/your_posts_1.json", opts).unwrap();
        z.write_all(posts_body.as_bytes()).unwrap();

        // friends section (the common single-wrapper-key → array shape). The
        // second friend's name is Meta mojibake for "café friend": the chars
        // U+00C3 U+00A9 are the Latin-1 mis-reading of UTF-8 bytes C3 A9 (é),
        // exactly as a real export stores it.
        z.start_file("connections/friends/friends.json", opts).unwrap();
        z.write_all(
            "{\"friends_v2\":[{\"name\":\"Sam Rivera\",\"timestamp\":1700000000},\
             {\"name\":\"caf\u{00c3}\u{00a9} friend\",\"timestamp\":1700000100}]}"
                .as_bytes(),
        )
        .unwrap();

        // reactions section.
        z.start_file("your_activity_across_facebook/reactions/likes_and_reactions_1.json", opts).unwrap();
        z.write_all(
            br#"{"reactions_v2":[{"timestamp":1700001000,"data":[{"reaction":{"reaction":"LIKE"}}],"title":"David likes a post."}]}"#,
        )
        .unwrap();

        // A media file (must be ignored, never copied).
        z.start_file("your_activity_across_facebook/posts/media/photo1.jpg", opts).unwrap();
        z.write_all(b"\xff\xd8\xff\xe0not-really-a-jpeg").unwrap();

        // Messenger stub (must be detected + skipped, NOT parsed).
        z.start_file("your_activity_across_facebook/messages/inbox/sam_abc123/message_1.json", opts).unwrap();
        z.write_all(br#"{"participants":[{"name":"Sam"}],"messages":[{"sender_name":"Sam","timestamp_ms":1700000000000,"content":"secret"}]}"#).unwrap();

        z.finish().unwrap();
        path
    }

    /// New-shape posts: a bare array. One text-only, one image-only (no
    /// `data`), one with a link `external_context`, one with `tags`. The
    /// fourth post's body is Meta mojibake for "café time 😀": "caf" + the
    /// Latin-1 mis-reading of é's UTF-8 (U+00C3 U+00A9) + a space + the
    /// four-char Latin-1 mis-reading of 😀's UTF-8 F0 9F 98 80 — proving the
    /// decode is recursive into post text.
    const POSTS_NEW: &str = "[\
      {\"timestamp\":1718000000,\"data\":[{\"post\":\"plain text post\"}],\"title\":\"David posted.\"},\
      {\"timestamp\":1718000100,\"attachments\":[{\"data\":[{\"media\":{\"uri\":\"posts/media/photo1.jpg\",\"description\":\"a sunset\",\"media_type\":\"image\"}}]}],\"title\":\"David posted a photo.\"},\
      {\"timestamp\":1718000200,\"data\":[{\"post\":\"check this out\"},{\"update_timestamp\":1718009999}],\"attachments\":[{\"data\":[{\"external_context\":{\"url\":\"https://example.com/article\",\"name\":\"Example Article Title\",\"source\":\"example.com\"}}]}]},\
      {\"timestamp\":1718000300,\"data\":[{\"post\":\"caf\u{00c3}\u{00a9} time \u{00f0}\u{009f}\u{0098}\u{0080}\"}],\"tags\":[{\"name\":\"Sam Rivera\"},{\"name\":\"Alex\"}]}\
    ]";

    /// Old-shape posts: the {"status_updates":[…]} wrapper, one text post.
    const POSTS_OLD: &str = "{\"status_updates\":[\
      {\"timestamp\":1719000000,\"data\":[{\"post\":\"legacy status update\"}],\"title\":\"David posted.\"}\
    ]}";

    #[test]
    fn imports_posts_to_the_social_contract() {
        let v = temp_vault("posts");
        let zip = export_zip("posts", POSTS_NEW);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("posts"), Some(&4), "four posts: {}", out.headline);

        // Partitioned by month of ts. 1718000000 = 2024-06-10 UTC.
        let raw = fs::read_to_string(v.root().join("social/facebook/2024-06.jsonl")).unwrap();
        let rows: Vec<Post> =
            raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(rows.len(), 4);

        // Sorted by ts on append; first is the text-only post.
        let text_post = rows.iter().find(|p| p.text == "plain text post").unwrap();
        assert_eq!(text_post.kind, "post");
        assert_eq!(text_post.source, "facebook");
        assert_eq!(text_post.title, "", "FB synthesized title never in Post::title");
        assert_eq!(text_post.extra.get("fb_title"), Some(&json!("David posted.")));
        assert_eq!(text_post.guid.len(), 64, "sha256 hex guid");

        // Image-only post: no `data` → no text, media metadata only.
        let image_post = rows.iter().find(|p| p.text.is_empty() && !p.media.is_empty()).unwrap();
        assert_eq!(image_post.media[0].r#type, "image");
        assert_eq!(image_post.media[0].url, "posts/media/photo1.jpg", "in-ZIP path, never copied");
        assert_eq!(image_post.media[0].alt, "a sunset");
        assert_eq!(image_post.extra.get("fb_title"), Some(&json!("David posted a photo.")));

        // Link post: external_context → url. The curated contract row carries
        // ONLY the normalized view — the link's name/source and the extra
        // `data[]` sibling timestamp are NOT bloated into it.
        let link_post = rows.iter().find(|p| p.text == "check this out").unwrap();
        assert_eq!(link_post.url, "https://example.com/article");
        let link_row = serde_json::to_value(link_post).unwrap();
        assert!(!link_row.to_string().contains("Example Article Title"), "name not in contract row");
        assert!(!link_row.to_string().contains("update_timestamp"), "data sibling not in contract row");

        // Tags post: tags[].name → tags, and mojibake decoded recursively.
        let tag_post = rows.iter().find(|p| !p.tags.is_empty()).unwrap();
        assert_eq!(tag_post.tags, vec!["Sam Rivera", "Alex"]);
        assert_eq!(tag_post.text, "café time 😀", "mojibake decoded in post text");

        // Raw fidelity: posts are ALSO kept losslessly under raw/posts.jsonl,
        // so fields the contract mapping drops survive. The link post's raw row
        // preserves external_context.name/.source AND the `data[]` sibling's
        // update_timestamp; mojibake is fixed there too.
        let raw_posts = fs::read_to_string(v.root().join("social/facebook/raw/posts.jsonl")).unwrap();
        let praw: Vec<Value> = raw_posts.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(praw.len(), 4, "every post kept in the raw layer");
        assert!(praw.iter().all(|r| r["section"] == json!("posts")));
        let link_raw = praw
            .iter()
            .find(|r| r["raw"]["timestamp"] == json!(1718000200))
            .unwrap();
        let ext = &link_raw["raw"]["attachments"][0]["data"][0]["external_context"];
        assert_eq!(ext["name"], json!("Example Article Title"), "dropped field survives in raw");
        assert_eq!(ext["source"], json!("example.com"), "dropped field survives in raw");
        assert_eq!(
            link_raw["raw"]["data"][1]["update_timestamp"],
            json!(1718009999),
            "data[] sibling survives in raw"
        );
        // Mojibake fixed in the raw post copy as well.
        let moji_raw = praw
            .iter()
            .find(|r| r["raw"]["timestamp"] == json!(1718000300))
            .unwrap();
        assert_eq!(moji_raw["raw"]["data"][0]["post"], json!("café time 😀"), "mojibake fixed in raw post");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn handles_old_status_updates_wrapper_shape() {
        let v = temp_vault("oldshape");
        let zip = export_zip("oldshape", POSTS_OLD);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("posts"), Some(&1), "old wrapper parsed: {}", out.headline);
        // 1719000000 = 2024-06-21 UTC.
        let raw = fs::read_to_string(v.root().join("social/facebook/2024-06.jsonl")).unwrap();
        assert!(raw.contains("legacy status update"));
        let _ = fs::remove_file(zip);
    }

    #[test]
    fn writes_raw_sections_full_fidelity_with_mojibake_fixed() {
        let v = temp_vault("raw");
        let zip = export_zip("raw", POSTS_NEW);
        let out = run(&v, &zip);
        assert!(out.counts.get("raw").unwrap() >= &3, "friends(2) + reactions(1): {}", out.headline);

        // friends.json → social/facebook/raw/friends.jsonl (version suffix stripped).
        let friends = fs::read_to_string(v.root().join("social/facebook/raw/friends.jsonl")).unwrap();
        let frows: Vec<Value> =
            friends.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(frows.len(), 2);
        assert_eq!(frows[0]["section"], json!("friends"));
        assert_eq!(frows[0]["raw"]["name"], json!("Sam Rivera"));
        assert!(frows[0].get("guid").and_then(Value::as_str).is_some());
        // The mojibake'd friend name is decoded in the raw layer too.
        assert_eq!(frows[1]["raw"]["name"], json!("café friend"), "mojibake fixed in raw");

        // reactions → social/facebook/raw/likes_and_reactions.jsonl.
        let reactions =
            fs::read_to_string(v.root().join("social/facebook/raw/likes_and_reactions.jsonl")).unwrap();
        assert!(reactions.contains("\"LIKE\""));

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn detects_and_skips_messenger_without_parsing() {
        let v = temp_vault("messenger");
        let zip = export_zip("messenger", POSTS_NEW);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("messenger_skipped"), Some(&1), "messenger detected");
        assert!(out.headline.contains("Messenger conversations detected"), "{}", out.headline);
        assert!(out.headline.contains("#72"), "{}", out.headline);

        // The Messenger content must NOT have been parsed into any vault file.
        assert!(!v.root().join("correspondence/facebook-messenger").exists());
        // No raw section named for the messenger thread, and no "secret" text anywhere.
        let raw_dir = v.root().join("social/facebook/raw");
        if raw_dir.exists() {
            for entry in fs::read_dir(&raw_dir).unwrap().flatten() {
                let body = fs::read_to_string(entry.path()).unwrap();
                assert!(!body.contains("secret"), "messenger content leaked: {}", entry.path().display());
            }
        }
        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reimport_dedupes_posts_and_raw() {
        let v = temp_vault("reimport");
        let zip = export_zip("reimport", POSTS_NEW);

        let first = run(&v, &zip);
        let posts_before = fs::read_to_string(v.root().join("social/facebook/2024-06.jsonl")).unwrap();
        let friends_before = fs::read_to_string(v.root().join("social/facebook/raw/friends.jsonl")).unwrap();
        let raw_posts_before = fs::read_to_string(v.root().join("social/facebook/raw/posts.jsonl")).unwrap();

        let second = run(&v, &zip);
        assert_eq!(second.counts.get("posts"), Some(&0), "all posts dedupe on re-import");
        assert_eq!(second.counts.get("raw"), Some(&0), "all raw items (incl. raw posts) dedupe on re-import");
        assert!(second.counts.get("duplicates").unwrap() >= first.counts.get("posts").unwrap());

        let posts_after = fs::read_to_string(v.root().join("social/facebook/2024-06.jsonl")).unwrap();
        let friends_after = fs::read_to_string(v.root().join("social/facebook/raw/friends.jsonl")).unwrap();
        let raw_posts_after = fs::read_to_string(v.root().join("social/facebook/raw/posts.jsonl")).unwrap();
        assert_eq!(posts_before, posts_after, "posts file unchanged on re-import");
        assert_eq!(friends_before, friends_after, "raw friends file unchanged on re-import");
        assert_eq!(raw_posts_before, raw_posts_after, "raw posts file unchanged on re-import");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn joins_the_social_manifest_and_hub_card() {
        let v = temp_vault("manifest");
        let zip = export_zip("manifest", POSTS_NEW);
        run(&v, &zip);

        // The manifest indexes it as a `social` source.
        let m = v.rebuild_manifest().unwrap();
        let social = m.domains.iter().find(|d| d.domain == "social").unwrap();
        assert!(social.sources.contains(&"facebook".to_string()));
        assert!(!social.spec.is_empty());

        // The hub knows it: a default-off card with an import box + the ack param.
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "facebook").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["zip"]);
        assert_eq!(import_info.params[0].key, "acknowledge");
        assert!(import_info.params[0].required, "ack is required");
        assert_eq!(card.last_data.as_deref(), Some("2024-06"));

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn old_post_lines_still_deserialize() {
        // Back-compat: a social line written sparsely (only required fields)
        // still reads as a Post, and a future unknown field is tolerated.
        let line = r#"{"ts":"2024-06-10T00:00:00-07:00","source":"facebook","guid":"abc","future":"x"}"#;
        let p: Post = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "abc");
        assert_eq!(p.kind, "");
        assert!(p.text.is_empty());
    }

    #[test]
    fn section_name_strips_version_and_shard_suffixes() {
        assert_eq!(section_name("connections/friends/friends_v2.json"), "friends");
        assert_eq!(section_name("a/b/reactions_1.json"), "reactions");
        assert_eq!(section_name("a/search_history.json"), "search_history");
        assert_eq!(section_name("a/your_topics_v2.json"), "your_topics");
        // No spurious stripping of a non-numeric tail.
        assert_eq!(section_name("a/group_posts.json"), "group_posts");
    }

    #[test]
    fn post_guid_is_injective_across_component_boundaries() {
        // Length-prefixing makes the hash injective: a literal `|` in the text
        // can't shift a component boundary. ("a|b","c") and ("a","b|c") must
        // hash differently (a naive `|`-join would collide them).
        assert_ne!(post_guid(0, "a|b", "c"), post_guid(0, "a", "b|c"));
        // Stable for the same inputs (re-import dedupe), and 64-hex.
        let g = post_guid(1718000000, "hello", "David posted.");
        assert_eq!(g, post_guid(1718000000, "hello", "David posted."));
        assert_eq!(g.len(), 64);
        // The timestamp participates: same text, different ts → different guid.
        assert_ne!(post_guid(1, "x", ""), post_guid(2, "x", ""));
    }
}
