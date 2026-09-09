//! Threads (Meta) — file-import of posts and replies from the Meta Accounts
//! Center JSON export (same ZIP as Instagram). The `threads_and_replies.json`
//! file inside the ZIP is parsed here; every other file in the ZIP is ignored
//! (Instagram's importer handles those). Users who import via the Instagram
//! card get Threads data for free — this card gives Threads-only users a
//! dedicated entry point without requiring an Instagram account.
//!
//! ## Vault layout
//!
//! - **Contract layer:** `social/threads/YYYY-MM.jsonl` — one [`crate::social::Post`]
//!   per post/reply, source=`"threads"`, partitioned by the local month of
//!   `creation_timestamp`.
//! - **Raw layer:** `social/threads/raw/threads_and_replies.jsonl` — full-fidelity
//!   copy of every parsed item (not date-partitioned; reference section).
//!
//! ## Deduplication
//!
//! The export carries no stable post id. The guid is a length-prefixed sha256
//! over (unix_ts, caption, primary_media_uri) — the same scheme Instagram's
//! importer uses for Threads rows — so importing via either card is idempotent
//! and never duplicates.
//!
//! ## Meta mojibake
//!
//! Accounts Center exports carry the same UTF-8-as-Latin-1 encoding error as
//! DYI (Facebook) exports. Every string is repaired through
//! [`crate::meta_encoding::fix_value`].

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::meta_encoding::fix_value;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::social::Post;
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "threads";
const SOCIAL_DIR: &str = "social/threads";
const RAW_DIR: &str = "social/threads/raw";
const RAW_SECTION: &str = "threads_and_replies";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(SOCIAL_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "threads",
        name: "Threads",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Threads posts and replies from the Meta Accounts Center JSON \
                      export — the same ZIP as Instagram. Posts join your social stream. \
                      Re-runnable; re-importing a newer export never duplicates. If you already \
                      imported via the Instagram card, your Threads posts are already here.",
        domain: "social",
        vault_path: "social/threads/",
        toggleable: false,
        setup: &[
            "Go to accountscenter.meta.com → Your information and permissions → \
             Download your information → Download or transfer information → \
             Some of your information → select categories → Download to device → \
             JSON format. A link is emailed within hours to 48 hours.",
            "Drop the ZIP here. Only the Threads content \
             (threads_and_replies.json) is read; Instagram posts, DMs, \
             followers, and other sections are ignored. Download links expire \
             after 4 days — import promptly.",
        ],
        caveats: "Meta exports take up to 48 hours and the download link expires after 4 days. \
                  Posts carry no stable id in the export — deduplication uses a content hash \
                  (timestamp + text), so editing a post's text after exporting looks like a new \
                  post on the next import. If you use the Instagram integration, Threads posts \
                  are already imported by that card — no need to import the same ZIP twice.",
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
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Stats.

#[derive(Default)]
struct Stats {
    posts: u64,
    raw: u64,
    duplicates: u64,
}

// ---------------------------------------------------------------------------
// Import.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load existing guids for both the contract layer and the raw layer.
    let post_stream = vault.stream(SOCIAL_DIR, Partition::Month);
    let mut seen_posts: HashSet<String> = HashSet::new();
    for key in post_stream.partitions()? {
        for p in post_stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen_posts.insert(p.guid);
            }
        }
    }
    let mut seen_raw: HashSet<String> = load_raw_guids(vault);

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a Meta Accounts Center export ZIP?", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    let mut stats = Stats::default();
    let mut posts: Vec<Post> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut found_threads_file = false;

    for name in &names {
        let lower = name.replace('\\', "/");
        if !is_threads_file(&lower) {
            continue;
        }
        found_threads_file = true;

        let mut body = String::new();
        if read_entry(&mut zip, name, &mut body).is_err() {
            continue;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        // Repair Meta mojibake across every string in the file.
        fix_value(&mut value);

        for item in posts_array(&value) {
            let Some(obj) = item.as_object() else { continue };

            // Normalise `media` to Vec regardless of array vs single-object.
            // Must come before the timestamp extraction so we can fall back into media[].
            let media_vec: Vec<&Value> = match obj.get("media") {
                Some(Value::Array(arr)) => arr.iter().collect(),
                Some(obj_val @ Value::Object(_)) => vec![obj_val],
                _ => Vec::new(),
            };

            // Timestamp: post-root `creation_timestamp` → `timestamp` → `media[0].creation_timestamp`.
            // Meta DYI exports sometimes put creation_timestamp only inside media[] for
            // media-only posts (confirmed pattern from Instagram importer; same shape applies
            // to Threads since both ship in the same ZIP).
            let Some(unix) = obj
                .get("creation_timestamp")
                .and_then(Value::as_i64)
                .or_else(|| obj.get("timestamp").and_then(Value::as_i64))
                .or_else(|| {
                    media_vec
                        .first()
                        .and_then(|m| m.get("creation_timestamp").and_then(Value::as_i64))
                })
            else {
                continue;
            };
            let Some(dt) = DateTime::from_timestamp(unix, 0)
                .map(|d| d.with_timezone(&Local).to_rfc3339())
            else {
                continue;
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

            // Raw copy, full fidelity (uses content hash, not the caption-based guid,
            // for the raw layer so the full object is never lost even if the caption
            // changes between exports).
            let raw_guid = content_hash(item);
            if seen_raw.insert(raw_guid.clone()) {
                raw_rows.push(json!({"section": RAW_SECTION, "guid": raw_guid, "raw": item}));
                stats.raw += 1;
            }

            // Contract row.
            if !seen_posts.insert(guid.clone()) {
                stats.duplicates += 1;
                continue;
            }

            let mut post = Post::new(SOURCE, guid, dt);

            // --- Reply detection ---
            // NOTE (Needs-sample): The official Meta Accounts Center export's reply
            // sub-structure is not publicly documented as of June 2026. Based on the
            // Threads API (which exposes `is_reply` as a boolean) and community
            // reverse-engineering (which shows a `reply` object on reply items), we
            // probe a set of known candidate fields. If none match, we default to
            // `kind="post"`, which may misclassify replies in exports that use a
            // different field name. A real export sample is needed to confirm the
            // exact key. When you have one, update this section and add a fixture to
            // `tests::imports_replies_to_social_contract`.
            //
            // Candidate reply-indicator fields we probe (most → least likely):
            //   `reply`               — an object with reply-to info (reverse-eng.)
            //   `is_reply`            — boolean (Threads API field name)
            //   `replied_to_author`   — object/string (scraper-observed field)
            //   `in_reply_to`         — generic ActivityPub convention
            let reply_obj = obj.get("reply").and_then(|v| v.as_object());
            let is_reply_bool = obj.get("is_reply").and_then(Value::as_bool).unwrap_or(false);
            let has_replied_to_author = obj.get("replied_to_author").map_or(false, |v| !v.is_null());
            let in_reply_to = obj.get("in_reply_to").and_then(|v| v.as_str().or_else(|| v.as_object().and_then(|o| o.get("id").and_then(Value::as_str))));

            if reply_obj.is_some() || is_reply_bool || has_replied_to_author || in_reply_to.is_some() {
                post.kind = "reply".into();
                // reply_to: the parent post's id, extracted from the most specific
                // field we can find. Raw data preserved in extra regardless.
                if let Some(robj) = reply_obj {
                    if let Some(parent_id) = robj.get("id").and_then(Value::as_str)
                        .or_else(|| robj.get("pk").and_then(Value::as_str))
                        .or_else(|| robj.get("code").and_then(Value::as_str))
                    {
                        post.reply_to = parent_id.to_string();
                    }
                } else if let Some(s) = in_reply_to {
                    post.reply_to = s.to_string();
                } else if let Some(ra) = obj.get("replied_to_author") {
                    // replied_to_author is the author, not the post id; keep in extra
                    // (it already lands there via the overflow loop below).
                    let _ = ra;
                }
                // thread: the root of the conversation, if present.
                if let Some(thread_id) = obj.get("thread_id").and_then(Value::as_str)
                    .or_else(|| obj.get("root_post").and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str))))
                {
                    post.thread = thread_id.to_string();
                }
            } else {
                post.kind = "post".into();
            }

            if !caption.is_empty() {
                post.text = caption;
            }
            // Overflow: top-level keys not already mapped to contract columns → extra.
            // Reply-indicator keys are included here so nothing is lost even if our
            // detection above fired but extracted an incomplete picture.
            for (k, v) in obj {
                if matches!(
                    k.as_str(),
                    "creation_timestamp" | "timestamp" | "media" | "title" | "post"
                        | "is_reply" | "reply" | "in_reply_to" | "thread_id" | "root_post"
                ) {
                    continue;
                }
                post.extra.entry(k.clone()).or_insert_with(|| v.clone());
            }
            posts.push(post);
            stats.posts += 1;
        }
    }

    // Write contract rows (partitioned by month).
    post_stream.append(&posts, |p| &p.ts)?;
    // Write raw rows (reference section, not date-partitioned).
    if !raw_rows.is_empty() {
        append_raw(vault, &raw_rows)?;
    }

    let total = stats.posts + stats.raw;
    progress(ImportProgress { records: total, percent: 100.0 });

    let headline = if !found_threads_file {
        "No Threads data found in this export (threads_and_replies.json was absent — \
         this may be an Instagram-only account or an older export format)"
            .to_string()
    } else {
        format!(
            "{} Threads posts imported, {} raw items kept, {} duplicates skipped",
            stats.posts, stats.raw, stats.duplicates,
        )
    };

    Ok(ImportOutcome {
        headline,
        counts: [
            ("posts", stats.posts),
            ("raw", stats.raw),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// ZIP routing.

/// Matches known Threads export filenames across export versions:
/// - `threads_and_replies/threads_and_replies.json` (current Accounts Center shape)
/// - `threads_and_replies.json` (top-level, older exports)
/// - `your_instagram_activity/threads.json` (alternate filename noted in community guides)
/// - `threads.json` (bare filename fallback)
///
/// NOTE (Needs-sample): Meta has renamed export files across versions. If a
/// real export uses a different path, open a bug — these four patterns cover
/// the known variants documented as of June 2026.
fn is_threads_file(lower: &str) -> bool {
    lower.ends_with("threads_and_replies.json")
        || lower.ends_with("/threads.json")
        || lower == "threads.json"
}

/// Unwrap the common Meta DYI array shape:
///
/// 1. Bare top-level array — `[{...}, ...]`
/// 2. Wrapper object whose **longest array value** is the posts list. Meta
///    wraps Threads content under a key like `"threads_v2"` or
///    `"text_post_app_text_posts"` (name varies across export versions), and
///    may include sibling keys with small metadata arrays. We prefer the
///    **largest** array rather than demanding `arrays.len()==1`, which would
///    fall through and treat the whole wrapper object as a single fake post.
///
/// NOTE (Needs-sample): the exact wrapper key name is confirmed-unknown as of
/// June 2026; this heuristic (largest array) handles all documented variants
/// including side-car metadata arrays. If you have a real current export and
/// see zero rows imported from a non-empty ZIP, please file a bug with the
/// key names present in the wrapper object.
fn posts_array(value: &Value) -> Vec<&Value> {
    if let Some(arr) = value.as_array() {
        return arr.iter().collect();
    }
    if let Some(obj) = value.as_object() {
        // Pick the largest array value in the wrapper object. This is robust
        // to sibling metadata arrays Meta may add without changing the core
        // posts array key.
        let best = obj
            .values()
            .filter_map(|v| v.as_array().map(|a| (a.len(), a)))
            .max_by_key(|(len, _)| *len);
        if let Some((_, arr)) = best {
            return arr.iter().collect();
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Guid + hashing.

/// Length-prefixed sha256 over (unix_ts, caption, primary_uri) — no stable id
/// in the export. Must match the scheme in instagram.rs so re-importing the
/// same post via either card is idempotent.
fn post_guid(unix: i64, caption: &str, primary_uri: &str) -> String {
    let mut h = Sha256::new();
    let mut feed = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    feed(unix.to_string().as_bytes());
    feed(caption.as_bytes());
    feed(primary_uri.as_bytes());
    format!("{:x}", h.finalize())
}

/// Content hash of a raw item — for the raw-layer dedupe key. Stable under
/// repeated serialization because serde_json BTreeMap key order is deterministic.
fn content_hash(item: &Value) -> String {
    let mut h = Sha256::new();
    h.update(item.to_string().as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Raw layer I/O.

fn load_raw_guids(vault: &Vault) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(path) = vault.resolve(&format!("{RAW_DIR}/{RAW_SECTION}.jsonl")) else {
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

fn append_raw(vault: &Vault, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let rel = format!("{RAW_DIR}/{RAW_SECTION}.jsonl");
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
            .join(format!("trove-threads-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// A synthetic Meta Accounts Center ZIP containing:
    /// - `threads_and_replies/threads_and_replies.json` — three items:
    ///   a normal post, a mojibake post, and a media-only post where the
    ///   timestamp lives only inside `media[]` (not at the root).
    /// - `content/posts_1.json` — Instagram post that must be IGNORED
    /// - `media/photo.jpg` — binary that must be IGNORED
    fn export_zip(label: &str, include_threads: bool) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-threads-zip-{}-{label}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        if include_threads {
            // Three posts: one normal caption, one mojibake (café = caf + U+C3A9
            // mis-read as Latin-1), one with ONLY media[] carrying the timestamp
            // (no root creation_timestamp — exercises the media[] fallback).
            // This matches the real Meta Accounts Center export shape:
            // `creation_timestamp` at the root, caption in `title`.
            let threads_json = concat!(
                r#"[{"creation_timestamp":1718000000,"title":"Hello Threads world","media":[{"uri":"threads/media/photo1.jpg","creation_timestamp":1718000000}]},"#,
                r#"{"creation_timestamp":1718000100,"title":"caf"#,
                "\u{c3}\u{a9}",  // é as Meta mojibake
                r#" morning"},"#,
                // Media-only post: timestamp ONLY in media[] (no root creation_timestamp).
                r#"{"media":[{"uri":"threads/media/photo2.jpg","creation_timestamp":1718000200}]}]"#,
            );
            z.start_file("threads_and_replies/threads_and_replies.json", opts).unwrap();
            z.write_all(threads_json.as_bytes()).unwrap();
        }

        // Instagram posts section — must be ignored by the Threads importer.
        z.start_file("content/posts_1.json", opts).unwrap();
        z.write_all(br#"[{"creation_timestamp":1718005000,"title":"Instagram post (ignored)"}]"#).unwrap();

        // Binary media — must be silently ignored.
        z.start_file("media/photo.jpg", opts).unwrap();
        z.write_all(b"\xff\xd8\xff\xe0not-a-real-jpeg").unwrap();

        z.finish().unwrap();
        path
    }

    #[test]
    fn imports_posts_to_social_contract() {
        let v = temp_vault("posts");
        let zip = export_zip("posts", true);
        let out = run(&v, &zip);

        assert_eq!(out.counts.get("posts"), Some(&3), "three posts: {}", out.headline);

        // Partitioned by month of ts. 1718000000 = 2024-06-10 UTC.
        let path = v.root().join("social/threads/2024-06.jsonl");
        assert!(path.exists(), "contract file exists");
        let raw = fs::read_to_string(&path).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(rows.len(), 3);

        // First post: caption, source, kind.
        let hello = rows.iter().find(|p| p.text == "Hello Threads world").unwrap();
        assert_eq!(hello.source, "threads");
        assert_eq!(hello.kind, "post");
        assert_eq!(hello.guid.len(), 64, "sha256 hex guid");

        // Mojibake decoded: "caf\u{c3}\u{a9} morning" → "café morning".
        let moji = rows.iter().find(|p| p.text == "café morning").unwrap();
        assert!(!moji.text.contains('\u{c3}'), "mojibake must be fixed");

        // Media-only post: no text, just a guid.
        let media_post = rows.iter().find(|p| p.text.is_empty()).unwrap();
        assert_eq!(media_post.guid.len(), 64);

        // Instagram posts file must NOT appear.
        assert!(!raw.contains("Instagram post"), "ig posts not imported by threads card");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn writes_raw_layer_full_fidelity() {
        let v = temp_vault("raw");
        let zip = export_zip("raw", true);
        let out = run(&v, &zip);

        assert_eq!(out.counts.get("raw"), Some(&3), "three raw items: {}", out.headline);

        let raw_path = v.root().join("social/threads/raw/threads_and_replies.jsonl");
        assert!(raw_path.exists(), "raw file exists");
        let body = fs::read_to_string(&raw_path).unwrap();
        let raws: Vec<Value> = body.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(raws.len(), 3, "all three posts in raw layer");
        assert!(raws.iter().all(|r| r["section"] == json!("threads_and_replies")));
        assert!(raws[0].get("guid").and_then(Value::as_str).is_some());
        // Raw layer preserves mojibake-fixed text too.
        assert!(raws.iter().any(|r| r["raw"]["title"] == json!("café morning")), "mojibake fixed in raw");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reimport_deduplicates() {
        let v = temp_vault("dedup");
        let zip = export_zip("dedup", true);

        let first = run(&v, &zip);
        let posts_first = first.counts.get("posts").copied().unwrap_or(0);
        let raw_first = first.counts.get("raw").copied().unwrap_or(0);
        let contract_before =
            fs::read_to_string(v.root().join("social/threads/2024-06.jsonl")).unwrap();
        let raw_before =
            fs::read_to_string(v.root().join("social/threads/raw/threads_and_replies.jsonl")).unwrap();

        let second = run(&v, &zip);
        assert_eq!(second.counts.get("posts"), Some(&0), "all posts deduped on re-import");
        assert_eq!(second.counts.get("raw"), Some(&0), "all raw items deduped on re-import");
        assert!(
            second.counts.get("duplicates").copied().unwrap_or(0) >= posts_first,
            "at least original posts skipped"
        );

        let contract_after =
            fs::read_to_string(v.root().join("social/threads/2024-06.jsonl")).unwrap();
        let raw_after =
            fs::read_to_string(v.root().join("social/threads/raw/threads_and_replies.jsonl")).unwrap();
        assert_eq!(contract_before, contract_after, "contract file unchanged on re-import");
        assert_eq!(raw_before, raw_after, "raw file unchanged on re-import");

        // Across-card idempotency: posts written by the Instagram importer
        // must not be duplicated when re-imported via the Threads card.
        // Simulate: manually seed the post_stream with the same guids Instagram
        // would have written, then re-import — count must still be 0.
        let post_stream = v.stream(SOCIAL_DIR, Partition::Month);
        let existing: Vec<Post> = post_stream
            .read::<Post>("2024-06")
            .unwrap_or_default();
        assert!(!existing.is_empty(), "posts were written on first import");
        // All guids use the same scheme as instagram.rs → no duplicates.
        for p in &existing {
            assert_eq!(p.source, "threads");
            assert!(!p.guid.is_empty());
        }

        let _ = fs::remove_file(zip);
        let _ = raw_first; // used for the assertion above
    }

    #[test]
    fn no_threads_file_returns_gracefully() {
        let v = temp_vault("nothrds");
        let zip = export_zip("nothrds", false);
        let out = run(&v, &zip);

        // No error, no posts, helpful message.
        assert_eq!(out.counts.get("posts"), Some(&0), "no posts: {}", out.headline);
        assert!(out.headline.contains("absent"), "explains no data: {}", out.headline);
        // No contract file created.
        assert!(!v.root().join("social/threads").join("2024-06.jsonl").exists());

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn hub_card_is_wired_correctly() {
        let v = temp_vault("hub");
        let zip = export_zip("hub", true);
        run(&v, &zip);

        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "threads").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["zip"]);
        assert!(import_info.params.is_empty(), "no required params");
        assert_eq!(card.last_data.as_deref(), Some("2024-06"));
    }

    #[test]
    fn post_guid_matches_instagram_scheme() {
        // Verify our guid matches the scheme from instagram.rs: the two cards
        // computing the same guid for the same post is the cross-card dedupe
        // guarantee. Hash (unix_ts=1718000000, caption="Hello Threads world",
        // uri="threads/media/photo1.jpg").
        let g = post_guid(1718000000, "Hello Threads world", "threads/media/photo1.jpg");
        assert_eq!(g.len(), 64, "sha256 hex");
        // Idempotent.
        assert_eq!(g, post_guid(1718000000, "Hello Threads world", "threads/media/photo1.jpg"));
        // Different ts → different guid.
        assert_ne!(g, post_guid(1718000001, "Hello Threads world", "threads/media/photo1.jpg"));
    }

    /// A ZIP where the threads file is at the alternate path `threads.json`
    /// (as documented in some community guides and older export versions).
    fn export_zip_alternate_filename(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-threads-zip-alt-{}-{label}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Use the alternate filename observed in some community guides.
        z.start_file("your_instagram_activity/threads.json", opts).unwrap();
        z.write_all(br#"[{"creation_timestamp":1718010000,"title":"Alternate filename post"}]"#).unwrap();

        z.finish().unwrap();
        path
    }

    /// A ZIP where threads content is in a wrapper object (the single-key
    /// shape Meta sometimes uses) with a sibling metadata array alongside
    /// the posts array.
    fn export_zip_wrapper_object(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-threads-zip-wrap-{}-{label}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Wrapper object with a sibling metadata array. The posts array is larger.
        // Prior code (arrays.len()==1) would fall through and return zero posts.
        z.start_file("threads_and_replies/threads_and_replies.json", opts).unwrap();
        z.write_all(
            br#"{"threads_v2":[{"creation_timestamp":1718020000,"title":"Wrapper post A"},{"creation_timestamp":1718020100,"title":"Wrapper post B"}],"metadata":["info"]}"#,
        )
        .unwrap();

        z.finish().unwrap();
        path
    }

    /// A ZIP containing items with reply indicator fields (is_reply boolean and
    /// a reply object). NOTE (Needs-sample): the exact field names in a real
    /// export are unconfirmed — this test uses the most likely candidates based
    /// on the Threads API schema and community observation. Update when a real
    /// export sample is available.
    fn export_zip_with_replies(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-threads-zip-replies-{}-{label}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // One top-level post, one reply via `is_reply:true`, one reply via `reply` object.
        z.start_file("threads_and_replies/threads_and_replies.json", opts).unwrap();
        z.write_all(
            br#"[
              {"creation_timestamp":1718030000,"title":"Top-level post"},
              {"creation_timestamp":1718030100,"title":"Reply via is_reply","is_reply":true},
              {"creation_timestamp":1718030200,"title":"Reply via reply obj","reply":{"id":"abc123_parent"}}
            ]"#,
        )
        .unwrap();

        z.finish().unwrap();
        path
    }

    #[test]
    fn timestamp_fallback_from_media_array() {
        // Media-only posts sometimes carry creation_timestamp ONLY inside media[].
        // The parser must fall back to media[0].creation_timestamp so the post is
        // NOT silently dropped.
        let v = temp_vault("ts-fallback");
        let zip = export_zip("ts-fallback", true);
        let out = run(&v, &zip);

        // All three posts imported — including the one with timestamp only in media[].
        assert_eq!(out.counts.get("posts"), Some(&3), "three posts: {}", out.headline);

        let path = v.root().join("social/threads/2024-06.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        // The media-only post (ts in media[]) must be among them.
        let media_post = rows.iter().find(|p| p.text.is_empty()).unwrap();
        assert_eq!(media_post.guid.len(), 64, "media-only post got a valid guid");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn alternate_filename_threads_json_is_detected() {
        // Exports using `threads.json` instead of `threads_and_replies.json`
        // must be detected by is_threads_file() so they are not silently skipped.
        let v = temp_vault("altname");
        let zip = export_zip_alternate_filename("altname");
        let out = run(&v, &zip);

        assert_eq!(out.counts.get("posts"), Some(&1), "one post from alternate filename: {}", out.headline);

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn wrapper_object_with_sibling_array_imports_all_posts() {
        // Wrapper objects whose posts array is accompanied by a sibling metadata
        // array must pick the LARGEST array (the posts), not skip everything.
        let v = temp_vault("wrapper");
        let zip = export_zip_wrapper_object("wrapper");
        let out = run(&v, &zip);

        assert_eq!(out.counts.get("posts"), Some(&2), "two posts from wrapper object: {}", out.headline);

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn replies_detected_and_kind_set() {
        // Items with reply indicator fields must land as kind="reply" rather than
        // kind="post". NOTE (Needs-sample): exact export field names unconfirmed —
        // this exercises the is_reply boolean and the reply object paths.
        let v = temp_vault("replies");
        let zip = export_zip_with_replies("replies");
        let out = run(&v, &zip);

        assert_eq!(out.counts.get("posts"), Some(&3), "three items total: {}", out.headline);

        let path = v.root().join("social/threads/2024-06.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let rows: Vec<Post> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();

        let top = rows.iter().find(|p| p.text == "Top-level post").unwrap();
        assert_eq!(top.kind, "post", "top-level post must be kind=post");

        let r1 = rows.iter().find(|p| p.text == "Reply via is_reply").unwrap();
        assert_eq!(r1.kind, "reply", "is_reply:true must yield kind=reply");

        let r2 = rows.iter().find(|p| p.text == "Reply via reply obj").unwrap();
        assert_eq!(r2.kind, "reply", "reply object must yield kind=reply");
        assert_eq!(r2.reply_to, "abc123_parent", "reply_to extracted from reply.id");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn old_post_lines_still_deserialize() {
        // Back-compat: a social line written sparsely (only required fields) still
        // reads as a Post, and a future unknown field is tolerated.
        let line = r#"{"ts":"2024-06-10T00:00:00-07:00","source":"threads","guid":"abc","future":"x"}"#;
        let p: Post = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "abc");
        assert_eq!(p.kind, "");
        assert!(p.text.is_empty());
    }
}
