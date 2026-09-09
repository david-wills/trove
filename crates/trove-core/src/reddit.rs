//! Reddit — official GDPR data export import (posts, comments, chat history).
//!
//! The only sanctioned personal-data path for a general user is the GDPR
//! **Data Request** ZIP (reddit.com/settings/data-request → GDPR option).
//! No auth, no network, no TCC at import time — standalone-clean.
//!
//! ## What lands where
//!
//! - **Comments** (`comments.csv`) → BOTH layers. Each row becomes one
//!   `kind:"comment"` [`social::Post`] under `social/reddit/YYYY-MM.jsonl`
//!   (the contract layer), and the raw CSV row is also written to
//!   `social/reddit/raw/comments.jsonl` full-fidelity.
//! - **Submissions / posts** (`posts.csv`) → BOTH layers. `kind:"post"` under
//!   `social/reddit/YYYY-MM.jsonl`, raw under `social/reddit/raw/posts.jsonl`.
//! - **Chat / DMs** (`messages_archive.csv` or `chat_history.csv`) →
//!   `correspondence/reddit/YYYY-MM.jsonl` per the ratified
//!   [`crate::correspondence`] contract. The correspondence slice is **opt-in**
//!   — the importer will not parse chat until the user has explicitly
//!   acknowledged the privacy gate. When `import_chats` is false the DM file
//!   is silently skipped — it is NOT written to the raw layer (private message
//!   bodies must not be stored without consent).
//! - **Everything else** (votes, saved, profile, etc.) → per-section raw files
//!   under `social/reddit/raw/<section>.jsonl`, full fidelity.
//!
//! ## Format
//!
//! The GDPR export is CSV (confirmed L4028–L4030 of the research doc).
//! `comments.csv` columns: `id,permalink,date,ip,subreddit,gildings,link,
//! parent,body,score`. The `ip` column is dropped at parse time per the brief.
//! Unknown extra columns are tolerated (future-proof).
//!
//! `posts.csv` columns per the official GDPR export (confirmed via
//! guilamu/reddit-gdpr-export-viewer source): `id,permalink,date,subreddit,
//! title,url,body,score`. Note: the body column is named `body` (not `text`
//! or `selftext`). An `ip` column may also be present — it is dropped.
//!
//! DM file: the current GDPR export names this `messages_archive.csv` with
//! columns `from,to,subject,body,date,id,permalink`. Legacy exports may use
//! `chat_history.csv` with columns `date,from,to,thread,body`. Both are
//! matched by substring; the `subject` field is captured if present.
//!
//! ## Dedupe
//!
//! Reddit's item ids are the [`social::Post::guid`] (fullname where available,
//! e.g. `t1_abc123`, else just the `id` column). Re-importing a newer export
//! never duplicates — the id set is loaded before writing.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::correspondence::Message;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportParam, ImportSpec, IntegrationDef};
use crate::social::Post;
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "reddit";
const DIR: &str = "social/reddit";
const RAW_DIR: &str = "social/reddit/raw";
const CORRESPONDENCE_SOURCE: &str = "reddit";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "reddit",
        name: "Reddit",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Reddit history — comments, posts, and (opt-in) DMs — from the \
                      official GDPR data export ZIP. Comments and posts join the unified social \
                      stream. Chat messages route to correspondence. Re-runnable; newer exports \
                      never duplicate.",
        domain: "social",
        vault_path: "social/reddit/",
        toggleable: false,
        setup: &[
            "reddit.com/settings/data-request → select the GDPR option → Request Data. \
             The export ZIP may take up to 30 days (often arrives within hours). \
             You will receive a download link by notification or email.",
            "Drop the downloaded ZIP here. Comments and posts are imported immediately. \
             To also import direct-message chat history, check the privacy acknowledgement below.",
        ],
        caveats: "Saved posts and votes are kept full-fidelity in the raw layer but are not \
                  surfaced in the social stream (they are not authored content). Chat messages \
                  require the privacy opt-in. The export covers only content not yet deleted \
                  from Reddit at export time — no recovery of deleted comments.",
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
        // 🔒 Correspondence opt-in — DM bodies are private; the hub renders
        // this as an optional field. Leave blank to import only posts/comments.
        key: "import_chats",
        label: "Import chat / DM history (optional)",
        placeholder: "Type 'yes' to import private message chat history into correspondence",
        required: false,
    }],
    run: run_import,
};

// ---------------------------------------------------------------------------
// CSV row structs — tolerant via `#[serde(default)]` on every optional field
// so future extra columns or missing columns never break the parse.

/// One row of `comments.csv`.
/// Documented columns (L4030): id, permalink, date, ip, subreddit, gildings,
/// link, parent, body, score. `ip` is dropped at parse time.
#[derive(Debug, Deserialize, Default)]
struct CommentRow {
    #[serde(rename = "id", default)]
    id: String,
    #[serde(rename = "permalink", default)]
    permalink: String,
    #[serde(rename = "date", default)]
    date: String,
    // ip column: present in export, must never be stored.
    #[serde(rename = "ip", default)]
    _ip: String,
    #[serde(rename = "subreddit", default)]
    subreddit: String,
    #[serde(rename = "gildings", default)]
    gildings: String,
    #[serde(rename = "link", default)]
    link: String,
    #[serde(rename = "parent", default)]
    parent: String,
    #[serde(rename = "body", default)]
    body: String,
    #[serde(rename = "score", default)]
    score: String,
}

/// One row of `posts.csv` (submissions).
/// Columns per the official Reddit GDPR export (confirmed via
/// guilamu/reddit-gdpr-export-viewer): id, permalink, date, subreddit,
/// title, url, body, score. Note: the submission body column is `body`
/// (not `text` or `selftext`). An `ip` column may be present — it is
/// dropped. `#[serde(default)]` on every field tolerates absent columns.
#[derive(Debug, Deserialize, Default)]
struct PostRow {
    #[serde(rename = "id", default)]
    id: String,
    #[serde(rename = "permalink", default)]
    permalink: String,
    #[serde(rename = "date", default)]
    date: String,
    // ip column: present in some export versions, must never be stored.
    #[serde(rename = "ip", default)]
    _ip: String,
    #[serde(rename = "subreddit", default)]
    subreddit: String,
    #[serde(rename = "title", default)]
    title: String,
    #[serde(rename = "url", default)]
    url: String,
    /// The submission body text. Column is named `body` in the official
    /// GDPR export — NOT `text` or `selftext`.
    #[serde(rename = "body", default)]
    body: String,
    #[serde(rename = "score", default)]
    score: String,
}

/// One row from a Reddit DM export file.
///
/// The current GDPR export uses `messages_archive.csv` with columns:
/// `from, to, subject, body, date, id, permalink` (confirmed via
/// guilamu/reddit-gdpr-export-viewer). Legacy exports may use
/// `chat_history.csv` with columns `date, from, to, thread, body`.
///
/// Both shapes are handled here: `subject` captures the DM subject line
/// from `messages_archive.csv`; `thread` captures the legacy thread
/// group id from `chat_history.csv`. `#[serde(default)]` on every field
/// means absent columns silently become empty strings.
#[derive(Debug, Deserialize, Default)]
struct ChatRow {
    #[serde(rename = "date", default)]
    date: String,
    #[serde(rename = "from", default)]
    from: String,
    #[serde(rename = "to", default)]
    to: String,
    /// Subject line — present in `messages_archive.csv`.
    #[serde(rename = "subject", default)]
    subject: String,
    /// Thread/conversation handle — present in legacy `chat_history.csv`.
    #[serde(rename = "thread", default)]
    thread: String,
    #[serde(rename = "body", default)]
    body: String,
    /// Message id — present in `messages_archive.csv`.
    #[serde(rename = "id", default)]
    id: String,
    /// Message permalink — present in `messages_archive.csv`.
    #[serde(rename = "permalink", default)]
    permalink: String,
}

// ---------------------------------------------------------------------------
// Runner

#[derive(Default)]
struct Stats {
    comments: u64,
    posts: u64,
    chats: u64,
    raw: u64,
    duplicates: u64,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let import_chats = params
        .get("import_chats")
        .map(|v| v.trim().eq_ignore_ascii_case("yes"))
        .unwrap_or(false);

    // Load existing social post guids for deduplication.
    let post_stream = vault.stream(DIR, Partition::Month);
    let mut seen_posts: HashSet<String> = HashSet::new();
    for key in post_stream.partitions()? {
        for p in post_stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen_posts.insert(p.guid);
            }
        }
    }

    // Load existing chat message guids for deduplication.
    let chat_stream = vault.stream(&format!("correspondence/{CORRESPONDENCE_SOURCE}"), Partition::Month);
    let mut seen_chats: HashSet<String> = HashSet::new();
    if import_chats {
        for key in chat_stream.partitions()? {
            for m in chat_stream.read::<Message>(&key)? {
                if !m.guid.is_empty() {
                    seen_chats.insert(m.guid);
                }
            }
        }
    }

    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a Reddit GDPR export ZIP?", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    let mut stats = Stats::default();
    let mut new_posts: Vec<Post> = Vec::new();
    let mut new_chats: Vec<Message> = Vec::new();
    // Raw rows keyed by section name.
    let mut raw_by_section: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut raw_seen: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    for name in &names {
        let lower = name.replace('\\', "/").to_ascii_lowercase();
        let file_name = lower.rsplit('/').next().unwrap_or(&lower);

        if !file_name.ends_with(".csv") {
            continue;
        }

        let mut body = String::new();
        if read_zip_entry(&mut zip, name, &mut body).is_err() {
            continue;
        }

        // Detect DM/chat files by substring first so the privacy gate can
        // apply unconditionally — the gate must prevent raw writes too.
        // Current export: "messages_archive.csv"; legacy: "chat_history.csv".
        let is_dm_file = file_name.contains("messages_archive")
            || file_name.contains("chat_history");

        if file_name == "comments.csv" {
            let section = "comments";
            let seen_raw = raw_seen
                .entry(section.to_string())
                .or_insert_with(|| load_raw_section_guids(vault, section));
            let raw_bucket = raw_by_section.entry(section.to_string()).or_default();

            // Build raw rows generically (full fidelity, ip dropped) while
            // also deserializing into typed structs for the contract layer.
            // Using a single reader pass with StringRecord avoids sync issues.
            let mut rdr = csv::Reader::from_reader(body.as_bytes());
            let headers: Vec<String> = match rdr.headers() {
                Ok(h) => h.iter().map(|s| s.to_string()).collect(),
                Err(_) => continue,
            };
            for record in rdr.records() {
                let Ok(record) = record else { continue };
                // Build typed row by deserializing from the record + headers.
                let row: CommentRow = match record.deserialize(Some(
                    &csv::StringRecord::from(headers.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                )) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if row.id.trim().is_empty() && row.body.trim().is_empty() {
                    continue;
                }

                // Raw row: build generically from actual headers, drop `ip`.
                let raw_row = record_to_generic_obj(&headers, &record, &["ip"]);
                let raw_guid = content_hash(&raw_row);
                if seen_raw.insert(raw_guid.clone()) {
                    raw_bucket.push(json!({"section": section, "guid": raw_guid, "raw": raw_row}));
                    stats.raw += 1;
                }

                // Contract row.
                let Some(post) = comment_to_post(&row) else { continue };
                if !seen_posts.insert(post.guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                new_posts.push(post);
                stats.comments += 1;
            }
        } else if file_name == "posts.csv" {
            let section = "posts";
            let seen_raw = raw_seen
                .entry(section.to_string())
                .or_insert_with(|| load_raw_section_guids(vault, section));
            let raw_bucket = raw_by_section.entry(section.to_string()).or_default();

            let mut rdr = csv::Reader::from_reader(body.as_bytes());
            let headers: Vec<String> = match rdr.headers() {
                Ok(h) => h.iter().map(|s| s.to_string()).collect(),
                Err(_) => continue,
            };
            for record in rdr.records() {
                let Ok(record) = record else { continue };
                let row: PostRow = match record.deserialize(Some(
                    &csv::StringRecord::from(headers.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                )) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if row.id.trim().is_empty() && row.title.trim().is_empty() {
                    continue;
                }

                // Raw row from actual CSV headers — full fidelity, ip dropped.
                let raw_row = record_to_generic_obj(&headers, &record, &["ip"]);
                let raw_guid = content_hash(&raw_row);
                if seen_raw.insert(raw_guid.clone()) {
                    raw_bucket.push(json!({"section": section, "guid": raw_guid, "raw": raw_row}));
                    stats.raw += 1;
                }

                let Some(post) = submission_to_post(&row) else { continue };
                if !seen_posts.insert(post.guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                new_posts.push(post);
                stats.posts += 1;
            }
        } else if is_dm_file {
            // 🔒 Privacy gate: DM files are skipped entirely (no raw write)
            // unless the user has explicitly opted in. Private message bodies
            // must never reach the vault without consent.
            if !import_chats {
                continue;
            }
            let mut rdr = csv::Reader::from_reader(body.as_bytes());
            for row in rdr.deserialize::<ChatRow>() {
                let Ok(row) = row else { continue };
                if row.date.trim().is_empty() && row.body.trim().is_empty() {
                    continue;
                }
                let Some(msg) = chat_to_message(&row) else { continue };
                if !seen_chats.insert(msg.guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                new_chats.push(msg);
                stats.chats += 1;
            }
        } else {
            // Any other CSV (votes, saved, profile, …) → raw layer, full fidelity.
            let section = file_name.strip_suffix(".csv").unwrap_or(file_name).to_string();
            let seen_raw = raw_seen
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, &section));
            let raw_bucket = raw_by_section.entry(section.clone()).or_default();

            let mut rdr = csv::Reader::from_reader(body.as_bytes());
            let headers: Vec<String> = match rdr.headers() {
                Ok(h) => h.iter().map(|s| s.to_string()).collect(),
                Err(_) => continue,
            };
            for record in rdr.records() {
                let Ok(record) = record else { continue };
                let mut obj = Map::new();
                for (k, v) in headers.iter().zip(record.iter()) {
                    if !k.is_empty() {
                        obj.insert(k.clone(), Value::String(v.to_string()));
                    }
                }
                if obj.is_empty() {
                    continue;
                }
                let raw_row = Value::Object(obj);
                let raw_guid = content_hash(&raw_row);
                if seen_raw.insert(raw_guid.clone()) {
                    raw_bucket.push(json!({"section": section, "guid": raw_guid, "raw": raw_row}));
                    stats.raw += 1;
                }
            }
        }
    }

    // Write social contract rows.
    post_stream.append(&new_posts, |p| &p.ts)?;

    // Write correspondence rows.
    if !new_chats.is_empty() {
        vault.append_messages(&new_chats)?;
    }

    // Write raw sections.
    for (section, rows) in &raw_by_section {
        if rows.is_empty() {
            continue;
        }
        append_raw_section(vault, section, rows)?;
    }

    let total = stats.comments + stats.posts + stats.chats + stats.raw;
    progress(ImportProgress { records: total, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{} comments + {} posts imported to social stream, {} raw items, {} chats to correspondence, {} duplicates skipped",
            stats.comments, stats.posts, stats.raw, stats.chats, stats.duplicates
        ),
        counts: [
            ("comments", stats.comments),
            ("posts", stats.posts),
            ("chats", stats.chats),
            ("raw", stats.raw),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Converters.

/// Parse a Reddit date string. The export uses "YYYY-MM-DD HH:MM:SS UTC" or
/// RFC3339. Returns an RFC3339 local timestamp for the vault.
fn parse_date(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Try "YYYY-MM-DD HH:MM:SS UTC" first (primary export shape).
    let stripped = s.strip_suffix(" UTC").unwrap_or(s);
    if let Ok(ndt) = NaiveDateTime::parse_from_str(stripped, "%Y-%m-%d %H:%M:%S") {
        let dt: DateTime<Local> = Utc.from_utc_datetime(&ndt).with_timezone(&Local);
        return Some(dt.to_rfc3339());
    }
    // Fallback: try RFC3339.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    None
}

/// Reddit fullname for a comment: `t1_<id>`.
fn comment_fullname(id: &str) -> String {
    let id = id.trim();
    if id.starts_with("t1_") {
        id.to_string()
    } else {
        format!("t1_{id}")
    }
}

/// Reddit fullname for a submission: `t3_<id>`.
fn submission_fullname(id: &str) -> String {
    let id = id.trim();
    if id.starts_with("t3_") {
        id.to_string()
    } else {
        format!("t3_{id}")
    }
}

/// Convert a `CommentRow` to a social [`Post`] contract row.
fn comment_to_post(row: &CommentRow) -> Option<Post> {
    let ts = parse_date(&row.date)?;
    let guid = comment_fullname(&row.id);
    let mut post = Post::new(SOURCE, guid, ts);
    post.kind = "comment".into();
    if !row.body.trim().is_empty() {
        post.text = row.body.trim().to_string();
    }
    if !row.permalink.trim().is_empty() {
        post.url = row.permalink.trim().to_string();
    }
    if !row.subreddit.trim().is_empty() {
        post.context = row.subreddit.trim().to_string();
    }
    if !row.link.trim().is_empty() {
        post.thread = row.link.trim().to_string();
    }
    if !row.parent.trim().is_empty() {
        post.reply_to = row.parent.trim().to_string();
    }
    // score, gildings → extra (engagement, not authored content).
    if !row.score.trim().is_empty() {
        post.extra.insert("score".into(), Value::String(row.score.trim().to_string()));
    }
    if !row.gildings.trim().is_empty() && row.gildings.trim() != "{}" && row.gildings.trim() != "0" {
        post.extra.insert("gildings".into(), Value::String(row.gildings.trim().to_string()));
    }
    Some(post)
}

/// Convert a `PostRow` to a social [`Post`] contract row.
fn submission_to_post(row: &PostRow) -> Option<Post> {
    let ts = parse_date(&row.date)?;
    let guid = submission_fullname(&row.id);
    let mut post = Post::new(SOURCE, guid, ts);
    post.kind = "post".into();
    if !row.title.trim().is_empty() {
        post.title = row.title.trim().to_string();
    }
    // Body column in the real export is `body` (confirmed via guilamu viewer).
    if !row.body.trim().is_empty() {
        post.text = row.body.trim().to_string();
    }
    if !row.permalink.trim().is_empty() {
        post.url = row.permalink.trim().to_string();
    }
    if !row.subreddit.trim().is_empty() {
        post.context = row.subreddit.trim().to_string();
    }
    // Link URL for link posts → extra (if permalink is used, keep url as the link target)
    if !row.url.trim().is_empty() && row.url.trim() != row.permalink.trim() {
        post.extra.insert("link_url".into(), Value::String(row.url.trim().to_string()));
    }
    if !row.score.trim().is_empty() {
        post.extra.insert("score".into(), Value::String(row.score.trim().to_string()));
    }
    Some(post)
}

/// Convert a `ChatRow` to a [`Message`] for the correspondence contract.
///
/// Handles both export shapes:
/// - `messages_archive.csv`: stable `id` available → use as guid directly.
///   `subject` captured into `msg.subject` if present. Conversation handle
///   derived from subject (no `thread` column in this format).
/// - `chat_history.csv` (legacy): no stable id → derive guid from content
///   hash. `thread` column used as conversation handle.
///
/// `from_me` is left false because the account username is not available at
/// import time. TODO: read the username from `statistics.csv` or
/// `account_gender.csv` in the same export ZIP, then set
/// `from_me = (row.from == username)`.
fn chat_to_message(row: &ChatRow) -> Option<Message> {
    let ts = parse_date(&row.date)?;

    // Prefer the stable message id from messages_archive.csv; fall back to
    // a content-hash for legacy chat_history.csv where no id column exists.
    let guid = if !row.id.trim().is_empty() {
        row.id.trim().to_string()
    } else {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(row.date.trim().as_bytes());
        h.update(b"|");
        h.update(row.from.trim().as_bytes());
        h.update(b"|");
        // Use thread (legacy) or subject as part of the conversation key.
        h.update(row.thread.trim().as_bytes());
        h.update(row.subject.trim().as_bytes());
        h.update(b"|");
        h.update(row.body.trim().as_bytes());
        format!("{:x}", h.finalize())
    };

    // Conversation handle: prefer subject (messages_archive), fall back to
    // thread (chat_history), then permalink root.
    let chat_key = if !row.subject.trim().is_empty() {
        row.subject.trim().to_string()
    } else if !row.thread.trim().is_empty() {
        row.thread.trim().to_string()
    } else if !row.permalink.trim().is_empty() {
        row.permalink.trim().to_string()
    } else {
        String::new()
    };

    let mut msg = Message::new(CORRESPONDENCE_SOURCE, ts);
    msg.guid = guid;
    msg.text = row.body.trim().to_string();
    msg.chat = chat_key;
    msg.sender = row.from.trim().to_string();
    // Subject line from messages_archive.csv — empty for legacy chat_history.csv.
    if !row.subject.trim().is_empty() {
        msg.subject = row.subject.trim().to_string();
    }
    // `from_me` is unknowable without the account username at import time;
    // left false. TODO: read username from statistics.csv in the export ZIP
    // and set from_me = (row.from == username) so sent messages are correct.
    if !row.to.trim().is_empty() {
        msg.to = row.to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    }
    Some(msg)
}

// ---------------------------------------------------------------------------
// Generic CSV-to-JSON helpers.

/// Build a JSON object from a single CSV `record` using the supplied
/// `headers`, dropping any column in `drop_cols` (case-sensitive) and
/// columns with empty headers. Unknown/extra columns are preserved —
/// this is the full-fidelity raw layer pass.
fn record_to_generic_obj(
    headers: &[String],
    record: &csv::StringRecord,
    drop_cols: &[&str],
) -> Value {
    let mut obj = Map::new();
    for (k, v) in headers.iter().zip(record.iter()) {
        if k.is_empty() || drop_cols.contains(&k.as_str()) {
            continue;
        }
        if !v.trim().is_empty() {
            obj.insert(k.clone(), Value::String(v.trim().to_string()));
        }
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Raw-section helpers (identical pattern to facebook.rs).

fn content_hash(item: &Value) -> String {
    use sha2::{Digest, Sha256};
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

/// Read one ZIP entry by name into `body`. Mirrors the facebook.rs pattern.
fn read_zip_entry(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    body: &mut String,
) -> Result<()> {
    body.clear();
    zip.by_name(name)
        .with_context(|| format!("entry {name}"))?
        .read_to_string(body)
        .with_context(|| format!("reading {name}"))?;
    Ok(())
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

// ---------------------------------------------------------------------------
// Test helpers for building synthetic ZIP fixtures.

#[cfg(test)]
fn make_zip(files: &[(&str, &[u8])]) -> std::path::PathBuf {
    // Each call gets a unique path (process-id + subsec nanos) so parallel
    // tests cannot overwrite each other's fixture ZIPs.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir()
        .join(format!("trove-reddit-fixture-{}-{nonce}.zip", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    for (name, data) in files {
        z.start_file(*name, opts).unwrap();
        std::io::Write::write_all(&mut z, data).unwrap();
    }
    z.finish().unwrap();
    path
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-reddit-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(vault: &Vault, path: &Path, import_chats: bool) -> ImportOutcome {
        let mut params = BTreeMap::new();
        if import_chats {
            params.insert("import_chats".to_string(), "yes".to_string());
        }
        (IMPORT.run)(vault, path, &params, &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixture CSVs — field names confirmed from research doc L4030.

    /// comments.csv: id, permalink, date, ip, subreddit, gildings, link, parent, body, score
    const COMMENTS_CSV: &[u8] = b"\
id,permalink,date,ip,subreddit,gildings,link,parent,body,score\n\
abc123,https://www.reddit.com/r/rust/comments/abc123/comment_here,2023-06-15 14:30:00 UTC,192.168.1.1,rust,{},t3_xyz789,t1_prev999,This is a test comment.,42\n\
def456,https://www.reddit.com/r/python/comments/def456/another_comment,2023-07-01 09:00:00 UTC,10.0.0.1,python,{},t3_qqq111,t3_qqq111,Another comment here.,7\n\
";

    /// posts.csv: id, permalink, date, ip, subreddit, title, url, body, score
    /// Body column is `body` (NOT `text`) per the official Reddit GDPR export
    /// (confirmed via guilamu/reddit-gdpr-export-viewer which reads post.body).
    const POSTS_CSV: &[u8] = b"\
id,permalink,date,ip,subreddit,title,url,body,score\n\
post001,https://www.reddit.com/r/rust/comments/post001/,2023-08-10 10:00:00 UTC,192.168.1.1,rust,My Rust Project,https://www.reddit.com/r/rust/comments/post001/,I built something cool in Rust.,150\n\
post002,https://www.reddit.com/r/worldnews/comments/post002/,2023-09-05 16:45:00 UTC,192.168.1.1,worldnews,Interesting article link,https://example.com/article,,20\n\
";

    /// chat_history.csv: legacy DM format — date, from, to, thread, body
    /// Body fields with commas are CSV-quoted to avoid field boundary splits.
    const CHAT_CSV: &[u8] = b"\
date,from,to,thread,body\n\
2023-10-01 12:00:00 UTC,user_alice,user_bob,thread_001,\"Hey how are you\"\n\
2023-10-01 12:05:00 UTC,user_bob,user_alice,thread_001,Doing great thanks!\n\
";

    /// messages_archive.csv: current GDPR export DM format.
    /// Columns: from, to, subject, body, date, id, permalink
    /// (confirmed via guilamu/reddit-gdpr-export-viewer).
    const MESSAGES_ARCHIVE_CSV: &[u8] = b"\
from,to,subject,body,date,id,permalink\n\
user_alice,user_bob,Hello thread,\"Hey how are you\",2023-10-01 12:00:00 UTC,msg_001,https://www.reddit.com/message/messages/msg_001\n\
user_bob,user_alice,Hello thread,Doing great thanks!,2023-10-01 12:05:00 UTC,msg_002,https://www.reddit.com/message/messages/msg_002\n\
";

    /// upvoted.csv: a minimal example of an "other" section.
    const UPVOTED_CSV: &[u8] = b"\
id,permalink\n\
t3_aaabbb,https://www.reddit.com/r/programming/comments/aaabbb/\n\
";

    #[test]
    fn imports_comments_to_social_contract() {
        let v = temp_vault("comments");
        let zip = make_zip(&[
            ("comments.csv", COMMENTS_CSV),
        ]);
        let out = run(&v, &zip, false);

        assert_eq!(out.counts.get("comments"), Some(&2), "{}", out.headline);

        // Both comments should land in social/reddit/YYYY-MM.jsonl.
        let posts: Vec<Post> = v
            .stream(DIR, Partition::Month)
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| v.stream(DIR, Partition::Month).read::<Post>(k).unwrap())
            .collect();
        assert_eq!(posts.len(), 2);

        let c1 = posts.iter().find(|p| p.guid == "t1_abc123").unwrap();
        assert_eq!(c1.kind, "comment");
        assert_eq!(c1.source, "reddit");
        assert_eq!(c1.text, "This is a test comment.");
        assert_eq!(c1.context, "rust");
        assert_eq!(c1.reply_to, "t1_prev999");
        assert_eq!(c1.thread, "t3_xyz789");
        assert_eq!(c1.url, "https://www.reddit.com/r/rust/comments/abc123/comment_here");
        // score in extra, never in a contract column.
        assert_eq!(c1.extra.get("score"), Some(&serde_json::Value::String("42".into())));

        // ip must NOT appear anywhere in the social contract output.
        let raw_json = serde_json::to_string(c1).unwrap();
        assert!(!raw_json.contains("192.168"), "ip leaked into contract row");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn imports_submissions_to_social_contract() {
        let v = temp_vault("posts");
        let zip = make_zip(&[
            ("posts.csv", POSTS_CSV),
        ]);
        let out = run(&v, &zip, false);
        assert_eq!(out.counts.get("posts"), Some(&2), "{}", out.headline);

        let posts: Vec<Post> = v
            .stream(DIR, Partition::Month)
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| v.stream(DIR, Partition::Month).read::<Post>(k).unwrap())
            .collect();
        assert_eq!(posts.len(), 2);

        let p1 = posts.iter().find(|p| p.guid == "t3_post001").unwrap();
        assert_eq!(p1.kind, "post");
        assert_eq!(p1.title, "My Rust Project");
        assert_eq!(p1.text, "I built something cool in Rust.");
        assert_eq!(p1.context, "rust");
        assert_eq!(p1.extra.get("score"), Some(&serde_json::Value::String("150".into())));

        // ip must not appear.
        let raw_json = serde_json::to_string(p1).unwrap();
        assert!(!raw_json.contains("192.168"), "ip leaked into contract row");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn chat_import_gated_requires_yes() {
        let v = temp_vault("chat_gate");
        let zip = make_zip(&[
            ("comments.csv", COMMENTS_CSV),
            ("chat_history.csv", CHAT_CSV),
        ]);
        // Without opt-in: chats must NOT be imported.
        let out = run(&v, &zip, false);
        assert_eq!(out.counts.get("chats"), Some(&0), "{}", out.headline);
        assert!(!v.root().join(format!("correspondence/{CORRESPONDENCE_SOURCE}")).exists(),
            "chat dir should not exist without opt-in");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn chat_import_works_with_opt_in() {
        let v = temp_vault("chat_optin");
        let zip = make_zip(&[
            ("chat_history.csv", CHAT_CSV),
        ]);
        let out = run(&v, &zip, true);
        assert_eq!(out.counts.get("chats"), Some(&2), "{}", out.headline);

        let msgs: Vec<Message> = v
            .stream(&format!("correspondence/{CORRESPONDENCE_SOURCE}"), Partition::Month)
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| {
                v.stream(&format!("correspondence/{CORRESPONDENCE_SOURCE}"), Partition::Month)
                    .read::<Message>(k)
                    .unwrap()
            })
            .collect();
        assert_eq!(msgs.len(), 2);

        let m1 = msgs.iter().find(|m| m.text == "Hey how are you").unwrap();
        assert_eq!(m1.source, CORRESPONDENCE_SOURCE);
        assert_eq!(m1.chat, "thread_001");
        assert_eq!(m1.sender, "user_alice");
        assert!(!m1.guid.is_empty());

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn raw_layer_written_full_fidelity_ip_dropped() {
        let v = temp_vault("raw");
        let zip = make_zip(&[
            ("comments.csv", COMMENTS_CSV),
            ("upvoted.csv", UPVOTED_CSV),
        ]);
        let out = run(&v, &zip, false);
        // raw = comments rows + upvoted rows.
        assert!(out.counts.get("raw").unwrap() >= &3, "{}", out.headline);

        let raw_comments =
            fs::read_to_string(v.root().join("social/reddit/raw/comments.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = raw_comments
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(rows.len(), 2);
        // body preserved.
        assert!(rows[0]["raw"]["body"].as_str().is_some());
        // ip must NOT be in the raw layer.
        assert!(rows[0].get("ip").is_none(), "ip in raw row wrapper");
        assert!(rows[0]["raw"].get("ip").is_none(), "ip in raw.raw");
        let raw_json = serde_json::to_string(&rows[0]).unwrap();
        assert!(!raw_json.contains("192.168"), "ip appeared in raw layer");

        // upvoted section → separate raw file.
        let raw_upvoted =
            fs::read_to_string(v.root().join("social/reddit/raw/upvoted.jsonl")).unwrap();
        assert!(raw_upvoted.contains("aaabbb"));

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reimport_deduplicates() {
        let v = temp_vault("dedup");
        let zip1 = make_zip(&[("comments.csv", COMMENTS_CSV)]);
        let out1 = run(&v, &zip1, false);
        assert_eq!(out1.counts.get("comments"), Some(&2), "{}", out1.headline);

        // Re-import the same file.
        let zip2 = make_zip(&[("comments.csv", COMMENTS_CSV)]);
        let out2 = run(&v, &zip2, false);
        assert_eq!(out2.counts.get("comments"), Some(&0), "no new imports on re-run: {}", out2.headline);
        assert_eq!(out2.counts.get("duplicates"), Some(&2), "{}", out2.headline);

        // Still exactly 2 rows in the vault.
        let all: Vec<Post> = v
            .stream(DIR, Partition::Month)
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| v.stream(DIR, Partition::Month).read::<Post>(k).unwrap())
            .collect();
        assert_eq!(all.len(), 2, "no duplication after re-import");

        let _ = fs::remove_file(zip1);
        let _ = fs::remove_file(zip2);
    }

    #[test]
    fn date_parse_utc_string() {
        let ts = parse_date("2023-06-15 14:30:00 UTC").unwrap();
        // Must be RFC3339.
        assert!(ts.contains("2023-06-15"), "date part preserved: {ts}");
        assert!(ts.contains('T'), "RFC3339 T separator: {ts}");
    }

    #[test]
    fn fullname_prefixing() {
        assert_eq!(comment_fullname("abc123"), "t1_abc123");
        assert_eq!(comment_fullname("t1_abc123"), "t1_abc123"); // already prefixed
        assert_eq!(submission_fullname("post001"), "t3_post001");
        assert_eq!(submission_fullname("t3_post001"), "t3_post001");
    }

    /// Regression: when import_chats=false, the DM file must NOT appear in the
    /// raw layer at all — private message bodies must not reach the vault
    /// without explicit opt-in (Defect 3 fix).
    #[test]
    fn chat_gate_no_raw_write_without_optin() {
        let v = temp_vault("chat_gate_raw");
        // Use both DM file names to confirm both are blocked.
        let zip = make_zip(&[
            ("comments.csv", COMMENTS_CSV),
            ("chat_history.csv", CHAT_CSV),
            ("messages_archive.csv", MESSAGES_ARCHIVE_CSV),
        ]);
        let out = run(&v, &zip, false);
        assert_eq!(out.counts.get("chats"), Some(&0), "{}", out.headline);

        let raw_dir = v.root().join("social/reddit/raw");
        // Neither DM file should produce a raw file under social/reddit/raw/.
        let chat_raw = raw_dir.join("chat_history.jsonl");
        let archive_raw = raw_dir.join("messages_archive.jsonl");
        assert!(!chat_raw.exists(), "chat_history.jsonl must not exist in raw without opt-in");
        assert!(!archive_raw.exists(), "messages_archive.jsonl must not exist in raw without opt-in");

        // Correspondence dir must be absent.
        assert!(!v.root().join(format!("correspondence/{CORRESPONDENCE_SOURCE}")).exists(),
            "correspondence dir must not exist without opt-in");

        // Confirm no chat body text leaked anywhere on disk.
        let vault_root = v.root().to_path_buf();
        let raw_json = if raw_dir.exists() {
            std::fs::read_dir(&raw_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter_map(|e| fs::read_to_string(e.path()).ok())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        assert!(!raw_json.contains("Hey how are you"), "chat body leaked to raw layer without opt-in");
        assert!(!raw_json.contains("Doing great thanks"), "chat body leaked to raw layer without opt-in");
        // The correspondence dir must also be absent.
        let corr_dir = vault_root.join(format!("correspondence/{CORRESPONDENCE_SOURCE}"));
        assert!(!corr_dir.exists());

        let _ = fs::remove_file(zip);
    }

    /// messages_archive.csv format (current GDPR export) is parsed correctly:
    /// stable `id` used as guid, `subject` captured, from/to/body preserved.
    #[test]
    fn messages_archive_csv_parsed_with_optin() {
        let v = temp_vault("messages_archive");
        let zip = make_zip(&[
            ("messages_archive.csv", MESSAGES_ARCHIVE_CSV),
        ]);
        let out = run(&v, &zip, true);
        assert_eq!(out.counts.get("chats"), Some(&2), "{}", out.headline);

        let msgs: Vec<Message> = v
            .stream(&format!("correspondence/{CORRESPONDENCE_SOURCE}"), Partition::Month)
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| {
                v.stream(&format!("correspondence/{CORRESPONDENCE_SOURCE}"), Partition::Month)
                    .read::<Message>(k)
                    .unwrap()
            })
            .collect();
        assert_eq!(msgs.len(), 2);

        let m1 = msgs.iter().find(|m| m.guid == "msg_001").unwrap();
        assert_eq!(m1.source, CORRESPONDENCE_SOURCE);
        assert_eq!(m1.text, "Hey how are you");
        assert_eq!(m1.sender, "user_alice");
        // subject used as conversation handle.
        assert_eq!(m1.chat, "Hello thread");
        assert!(!m1.guid.is_empty());

        let _ = fs::remove_file(zip);
    }

    /// Posts raw layer must store `body` (not `text`) and preserve unknown
    /// extra columns — full fidelity, ip dropped (Defect 1 + Defect 4 fix).
    #[test]
    fn posts_raw_uses_body_column_and_is_full_fidelity() {
        let v = temp_vault("posts_raw");
        let zip = make_zip(&[("posts.csv", POSTS_CSV)]);
        let out = run(&v, &zip, false);
        assert_eq!(out.counts.get("posts"), Some(&2), "{}", out.headline);

        let raw_posts =
            fs::read_to_string(v.root().join("social/reddit/raw/posts.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = raw_posts
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(rows.len(), 2);

        // body column preserved in raw layer (was broken when column was `text`).
        let first_body = rows.iter().find(|r| {
            r["raw"]["body"].as_str().map(|s| !s.is_empty()).unwrap_or(false)
        });
        assert!(first_body.is_some(), "raw posts must have non-empty `body` for self-posts");

        // ip must NOT be in the raw layer.
        let raw_json = serde_json::to_string(&rows[0]).unwrap();
        assert!(!raw_json.contains("192.168"), "ip appeared in posts raw layer");

        let _ = fs::remove_file(zip);
    }
}
