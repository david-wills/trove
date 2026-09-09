//! Snapchat — import of the official My Data export ZIP.
//!
//! ## Export layout
//!
//! accounts.snapchat.com → My Data → request export (24-48 h, up to 7 days).
//! The delivered ZIP contains:
//!
//! ```text
//! html/index.html               (HTML viewer — ignored)
//! json/
//!   account.json
//!   chat_history.json           ← saved chats
//!   memories_history.json       ← Memories (saved photos/videos)
//!   snap_history.json           ← snap send/receive metadata (no content)
//!   friends.json
//!   location_history.json
//!   …                           (login/search/purchase/bitmoji — raw only)
//! chat_media/
//!   <id>_<contact>_<mediaid>.<ext>   ← media files (ignored — URL-only vault rule)
//! memories/                     ← referenced by URL only in memories_history.json
//! ```
//!
//! Ephemeral snap *content* is deleted by design — only metadata is exported.
//! Media files in `chat_media/` and `memories/` are never copied into the vault.
//!
//! ## chat_history.json schema (confirmed from community tooling)
//!
//! **2024+ format** — flat dict of contact → messages:
//! ```json
//! {
//!   "ContactName": [
//!     {
//!       "From":               "alice",
//!       "IsSender":           true,
//!       "Created":            "2024-01-20 06:33:05 UTC",
//!       "Created(microseconds)": 1705732385472,
//!       "Content":            "Hey!",
//!       "Media Type":         "TEXT",
//!       "Media IDs":          null
//!     }
//!   ]
//! }
//! ```
//!
//! NOTE: despite the field name `Created(microseconds)`, real exports contain
//! epoch **milliseconds** (13 digits, e.g. `1705732385472`). Microseconds would
//! be 16 digits (`1705732385472000`). Verified across multiple community parsers
//! (raleighlittles/snapchat-dl, Tikolu). Prefer the `Created` ISO string; use
//! the numeric field only as a fallback (already in ms, no division needed).
//!
//! **Pre-2024 format** — separate received/sent arrays:
//! ```json
//! {
//!   "Received Chat History": [
//!     { "From": "alice", "To": "me", "Created": "2022-05-01 12:00:00 UTC",
//!       "Text": "Hello", "Media Type": "TEXT" }
//!   ],
//!   "Sent Chat History": [
//!     { "From": "me", "To": "alice", "Created": "2022-05-01 12:01:00 UTC",
//!       "Text": "Hi back", "Media Type": "TEXT" }
//!   ]
//! }
//! ```
//! Pre-2024: body in `Text`, no numeric timestamp, `From`/`To` instead of contact key.
//!
//! `Media Type` is `"TEXT"` | `"NOTE"` (voice) | `"MEDIA"`.
//!
//! ## memories_history.json schema (confirmed from community tooling)
//!
//! ```json
//! {
//!   "Saved Media": [
//!     {
//!       "Date":          "2023-11-14 10:00:00 UTC",
//!       "Media Type":    "Image",
//!       "Location":      "Latitude, Longitude: 37.77, -122.41",
//!       "Download Link": "https://sc-cdn.snapchat.com/..."
//!     }
//!   ]
//! }
//! ```
//!
//! The canonical field is `"Download Link"` (used by Tikolu, noelaridan, dustinrouillard).
//! Some exports/tools also use `"Media Download Url"` — both are checked.
//! `Media Type` is `"Image"` | `"Video"`.
//! `Location` may be empty string, or `"Latitude, Longitude: 0.0, 0.0"` for no geotag.
//!
//! ## Vault mapping
//!
//! | Data | Path |
//! |------|------|
//! | Raw JSON files | `social/snapchat/raw/<export-date>/` |
//! | Saved chat messages | `correspondence/snapchat/YYYY-MM.jsonl` |
//! | Memories metadata | `photos/snapchat/YYYY-MM.jsonl` |
//!
//! Snap history and other categories land raw-only (no standard contract fits
//! bare send/receive metadata without message content).
//!
//! ## Dedupe
//!
//! - Chat messages: `sha256(contact | created_us | from | content)` — no
//!   stable id in the export, mirrors facebook_messenger.rs.
//! - Memories: sha256 of `(date | location | download_url)` — the
//!   `"Media Download Url"` field is the closest unique identifier per asset.
//! - Re-importing the same ZIP is a pure no-op.
//!
//! ## Privacy
//!
//! Message bodies + location history are sensitive; an opt-in acknowledgement
//! param is required.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use sha2::{Digest, Sha256};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::photos::Photo;
use crate::registry::{Behavior, ImportOutcome, ImportParam, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "snapchat";
const CORRESPONDENCE_DIR: &str = "correspondence/snapchat";
const PHOTOS_DIR: &str = "photos/snapchat";
const RAW_BASE: &str = "social/snapchat/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime_recursive(&vault.root().join(CORRESPONDENCE_DIR))
        .or_else(|| crate::registry::newest_mtime_recursive(&vault.root().join(PHOTOS_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "snapchat",
        name: "Snapchat",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Snapchat Memories, saved chats, and account history \
                      from the official My Data export at accounts.snapchat.com. \
                      Ephemeral snap content is gone by design — the export captures \
                      saved messages, Memories metadata, and account data. \
                      Re-runnable; re-importing never duplicates.",
        domain: "correspondence",
        vault_path: "correspondence/snapchat/",
        toggleable: false,
        setup: &[
            "accounts.snapchat.com → My Data (or in-app Settings → My Data). \
             Select the data categories you want (Saved Chat History, Memories, etc.). \
             The export ZIP is typically ready within 24–48 hours.",
            "Drop the ZIP here. Message bodies are sensitive; import only if you \
             intend to store them in your private vault.",
        ],
        caveats: "Ephemeral snaps cannot be recovered — only Memories (photos/videos \
                  you explicitly saved) and Saved Chat History are meaningful. \
                  Media files are referenced by URL in the JSON and are not downloaded. \
                  If a category was not selected at export time, those rows are simply \
                  absent — the importer handles missing categories gracefully.",
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
        key: "acknowledge",
        label: "Privacy acknowledgement",
        placeholder: "Type 'yes' to confirm you want your Snapchat history stored in your vault",
        required: true,
    }],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Stats

#[derive(Default)]
struct Stats {
    chats: u64,
    conversations: HashSet<String>,
    memories: u64,
    raw_files: u64,
    duplicates: u64,
}

// ---------------------------------------------------------------------------
// Main importer

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load already-stored guids for both sinks so re-imports are no-ops.
    let mut chat_seen = vault.correspondence_guids(SOURCE)?;
    let mut photo_seen = load_photo_guids(vault)?;

    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a Snapchat My Data export ZIP?", path.display()))?;

    // Determine the export date for the raw layer folder.
    // Use today's date as a stable folder name; repeated imports of the SAME
    // ZIP land in the same folder and are deduplicated by content hash.
    let export_date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let raw_dir = format!("{RAW_BASE}/{export_date}");

    // Load existing raw hashes to deduplicate on re-import.
    let mut raw_seen = load_raw_hashes(vault, &raw_dir)?;

    let mut stats = Stats::default();
    let mut all_messages: Vec<Message> = Vec::new();
    let mut all_photos: Vec<Photo> = Vec::new();

    // Collect entry names first (ZipArchive cannot be borrowed mutably twice).
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    let total = names.len() as f32;
    for (idx, name) in names.iter().enumerate() {
        // Skip macOS metadata entries.
        if name.starts_with("__MACOSX") || name.contains("/.") {
            continue;
        }
        // Skip non-JSON; never touch media files.
        if !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }

        let mut body = String::new();
        if zip
            .by_name(name)
            .with_context(|| format!("opening entry {name}"))?
            .read_to_string(&mut body)
            .is_err()
        {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(&body) else {
            continue;
        };

        // ---- Raw layer (full fidelity, deduped by content hash) ----
        write_raw(vault, &raw_dir, name, &value, &mut raw_seen, &mut stats)?;

        // ---- Contract layers ----
        let lower_name = name.to_ascii_lowercase();
        let leaf = lower_name.rsplit('/').next().unwrap_or(&lower_name);

        if leaf == "chat_history.json" {
            parse_chats(&value, &mut all_messages, &mut chat_seen, &mut stats);
        } else if leaf == "memories_history.json" {
            parse_memories(&value, &mut all_photos, &mut photo_seen, &mut stats);
        }

        if idx % 5 == 0 {
            progress(ImportProgress {
                records: stats.chats + stats.memories,
                percent: (idx as f32 / total * 90.0_f32).min(90.0_f32),
            });
        }
    }

    // Flush to vault.
    vault.append_messages(&all_messages)?;
    vault
        .stream(PHOTOS_DIR, Partition::Month)
        .append(&all_photos, |p| &p.ts)?;

    progress(ImportProgress {
        records: stats.chats + stats.memories,
        percent: 100.0,
    });

    Ok(ImportOutcome {
        headline: format!(
            "{} chat messages, {} memories imported from {} conversations, {} duplicates skipped",
            stats.chats,
            stats.memories,
            stats.conversations.len(),
            stats.duplicates,
        ),
        counts: [
            ("chat_messages", stats.chats),
            ("memories", stats.memories),
            ("conversations", stats.conversations.len() as u64),
            ("raw_files", stats.raw_files),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Chat history parser

fn parse_chats(
    value: &Value,
    out: &mut Vec<Message>,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    let Some(obj) = value.as_object() else { return };

    // Detect pre-2024 format: top-level keys are "Received Chat History" /
    // "Sent Chat History" (raleighlittles, verdie-g parsers branch on this).
    let pre2024 = obj.contains_key("Received Chat History")
        || obj.contains_key("Sent Chat History");

    if pre2024 {
        parse_chats_pre2024(obj, out, seen, stats);
    } else {
        parse_chats_2024(obj, out, seen, stats);
    }
}

/// Pre-2024 format: { "Received Chat History": [...], "Sent Chat History": [...] }
/// Each item: From, To, Created (ISO), Text, Media Type.  No numeric timestamp.
fn parse_chats_pre2024(
    obj: &serde_json::Map<String, Value>,
    out: &mut Vec<Message>,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    for array_name in &["Received Chat History", "Sent Chat History"] {
        let from_me = *array_name == "Sent Chat History";
        let Some(msgs) = obj.get(*array_name).and_then(Value::as_array) else { continue };

        for msg in msgs {
            let Some(o) = msg.as_object() else { continue };

            let created = o.get("Created").and_then(Value::as_str).unwrap_or("").to_string();
            if created.is_empty() {
                continue;
            }
            let ts = parse_snapchat_date(&created);

            // Derive contact: counterpart field
            let contact = if from_me {
                o.get("To").and_then(Value::as_str).unwrap_or("").to_string()
            } else {
                o.get("From").and_then(Value::as_str).unwrap_or("").to_string()
            };
            let from = o.get("From").and_then(Value::as_str).unwrap_or("").to_string();
            // Pre-2024 uses "Text" not "Content".
            let content = o.get("Text").and_then(Value::as_str).unwrap_or("").to_string();
            let media_type = o.get("Media Type").and_then(Value::as_str).unwrap_or("TEXT").to_string();

            // Use "created" ISO string as timestamp key for guid stability.
            let guid = chat_guid(&contact, &created, &from, &content);
            if !seen.insert(guid.clone()) {
                stats.duplicates += 1;
                continue;
            }

            let mut m = Message::new(SOURCE, ts);
            m.guid = guid;
            m.chat = contact.clone();
            m.from_me = from_me;
            if !from.is_empty() {
                if from_me {
                    m.sender_name = from;
                } else {
                    m.sender = from.clone();
                    m.sender_name = from;
                }
            }
            m.text = content;
            m.service = "Snapchat".into();

            if media_type != "TEXT" {
                m.attachments.push(AttachmentMeta {
                    name: format!("snap-{}", media_type.to_lowercase()),
                    mime: if media_type == "NOTE" { "audio" } else { "media" }.into(),
                    bytes: 0,
                });
            }

            out.push(m);
            stats.chats += 1;
            if !contact.is_empty() {
                stats.conversations.insert(contact);
            }
        }
    }
}

/// 2024+ format: flat dict { "ContactName": [{messages}], … }
fn parse_chats_2024(
    obj: &serde_json::Map<String, Value>,
    out: &mut Vec<Message>,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    for (contact, messages) in obj {
        let Some(msgs) = messages.as_array() else { continue };

        for msg in msgs {
            let Some(o) = msg.as_object() else { continue };

            // Prefer the ISO "Created" string for timestamp; use numeric field as fallback.
            // NOTE: despite the name "Created(microseconds)", real Snapchat exports
            // contain epoch MILLISECONDS (13 digits). No division needed.
            let ts = if let Some(iso) = o.get("Created").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                parse_snapchat_date(iso)
            } else {
                // Numeric fallback — already milliseconds.
                let ts_ms = o.get("Created(microseconds)")
                    .and_then(|v| v.as_i64())
                    .or_else(|| {
                        o.get("Created(microseconds)")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<i64>().ok())
                    })
                    .unwrap_or(0);
                if ts_ms == 0 {
                    continue;
                }
                ts_ms_to_local(ts_ms)
            };

            // Use numeric field for guid stability if present; otherwise ISO string.
            let ts_key = o.get("Created(microseconds)")
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string())
                .or_else(|| {
                    o.get("Created(microseconds)")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| {
                    o.get("Created").and_then(Value::as_str).unwrap_or("").to_string()
                });

            let from = o
                .get("From")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let content = o
                .get("Content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let media_type = o
                .get("Media Type")
                .and_then(Value::as_str)
                .unwrap_or("TEXT")
                .to_string();
            let is_sender = o
                .get("IsSender")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            // Media IDs: string or array-of-strings.
            let media_ids: Option<String> = o.get("Media IDs").and_then(|v| {
                if let Some(s) = v.as_str() {
                    if s.is_empty() { None } else { Some(s.to_string()) }
                } else if let Some(arr) = v.as_array() {
                    let ids: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                    if ids.is_empty() { None } else { Some(ids.join(",")) }
                } else {
                    None
                }
            });

            // guid = sha256(contact | ts_key | from | content)
            let guid = chat_guid(contact, &ts_key, &from, &content);
            if !seen.insert(guid.clone()) {
                stats.duplicates += 1;
                continue;
            }

            let mut m = Message::new(SOURCE, ts);
            m.guid = guid;
            m.chat = contact.clone();
            m.from_me = is_sender;
            if !from.is_empty() {
                if is_sender {
                    // Sender is the vault owner — leave m.sender empty.
                    m.sender_name = from;
                } else {
                    m.sender = from.clone();
                    m.sender_name = from;
                }
            }
            m.text = content;
            m.service = "Snapchat".into();

            // Attachments: if not TEXT, note the media type.
            if media_type != "TEXT" && (!m.text.is_empty() || media_ids.is_some()) {
                let att_name = if let Some(id) = &media_ids {
                    id.clone()
                } else {
                    format!("snap-{}", &media_type.to_lowercase())
                };
                let mime = if media_type == "NOTE" {
                    "audio"
                } else {
                    "media"
                };
                m.attachments.push(AttachmentMeta {
                    name: att_name,
                    mime: mime.into(),
                    bytes: 0,
                });
            } else if media_type != "TEXT" {
                // No content and no id — still a media message worth recording.
                m.attachments.push(AttachmentMeta {
                    name: format!("snap-{}", media_type.to_lowercase()),
                    mime: "media".into(),
                    bytes: 0,
                });
            }

            out.push(m);
            stats.chats += 1;
            stats.conversations.insert(contact.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Memories parser

fn parse_memories(
    value: &Value,
    out: &mut Vec<Photo>,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    // memories_history.json: { "Saved Media": [{Date, Media Type, Location, Media Download Url}] }
    let items = value
        .get("Saved Media")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    for item in items {
        let Some(obj) = item.as_object() else { continue };

        let date_str = obj.get("Date").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let media_type = obj
            .get("Media Type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let location = obj
            .get("Location")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // "Download Link" is the canonical field name (Tikolu, noelaridan, dustinrouillard);
        // "Media Download Url" appears in some newer/third-party exports — accept both.
        let url = obj
            .get("Download Link")
            .or_else(|| obj.get("Media Download Url"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if date_str.is_empty() {
            continue;
        }

        // guid = sha256(date | location | url)
        let guid = memory_guid(&date_str, &location, &url);
        if !seen.insert(guid.clone()) {
            stats.duplicates += 1;
            continue;
        }

        let ts = parse_snapchat_date(&date_str);

        let kind = match media_type.to_ascii_lowercase().as_str() {
            "video" => "video",
            _ => "photo",
        };

        let mut p = Photo::new(SOURCE, guid, &ts);
        p.kind = kind.into();

        // Parse "Latitude, Longitude: X.X, Y.Y" → lat/lon.
        if !location.is_empty() {
            if let Some((lat, lon)) = parse_location(&location) {
                p.lat = Some(lat);
                p.lon = Some(lon);
            }
        }

        // Store the CDN URL in extra (never download it).
        if !url.is_empty() {
            p.extra.insert("download_url".into(), Value::String(url));
        }

        out.push(p);
        stats.memories += 1;
    }
}

// ---------------------------------------------------------------------------
// Raw layer

/// Write one JSON entry to the raw vault folder, deduped by content hash.
fn write_raw(
    vault: &Vault,
    raw_dir: &str,
    zip_entry_name: &str,
    value: &Value,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) -> Result<()> {
    use std::io::Write;

    // Derive a file stem from the ZIP entry (e.g. "json/chat_history.json" → "chat_history").
    let leaf = zip_entry_name.rsplit('/').next().unwrap_or(zip_entry_name);
    let stem = leaf.rfind('.').map(|i| &leaf[..i]).unwrap_or(leaf);

    let canonical = serde_json::to_string(value).unwrap_or_default();
    let hash = {
        let mut h = Sha256::new();
        h.update(stem.as_bytes());
        h.update(b":");
        h.update(canonical.as_bytes());
        format!("{:x}", h.finalize())
    };
    if !seen.insert(hash) {
        return Ok(());
    }

    let rel = format!("{raw_dir}/{stem}.json");
    let path = vault.resolve(&rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write the raw file as pretty JSON (one file per category, not JSONL).
    let pretty = serde_json::to_string_pretty(value)?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("opening raw {rel}"))?
        .write_all(pretty.as_bytes())?;

    stats.raw_files += 1;
    Ok(())
}

/// Load content hashes already present in the raw export-date folder.
fn load_raw_hashes(vault: &Vault, raw_dir: &str) -> Result<HashSet<String>> {
    let mut seen = HashSet::new();
    let dir_path = match vault.resolve(raw_dir) {
        Ok(p) => p,
        Err(_) => return Ok(seen),
    };
    let Ok(entries) = std::fs::read_dir(&dir_path) else {
        return Ok(seen);
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Ok(body) = std::fs::read_to_string(&p) {
                if let Ok(val) = serde_json::from_str::<Value>(&body) {
                    let canonical = serde_json::to_string(&val).unwrap_or_default();
                    let hash = {
                        let mut h = Sha256::new();
                        h.update(stem.as_bytes());
                        h.update(b":");
                        h.update(canonical.as_bytes());
                        format!("{:x}", h.finalize())
                    };
                    seen.insert(hash);
                }
            }
        }
    }
    Ok(seen)
}

// ---------------------------------------------------------------------------
// Photo guid helpers

fn load_photo_guids(vault: &Vault) -> Result<HashSet<String>> {
    let stream = vault.stream(PHOTOS_DIR, Partition::Month);
    let mut set = HashSet::new();
    let Ok(parts) = stream.partitions() else {
        return Ok(set);
    };
    for key in parts {
        for p in stream.read::<Photo>(&key)? {
            if !p.guid.is_empty() {
                set.insert(p.guid);
            }
        }
    }
    Ok(set)
}

// ---------------------------------------------------------------------------
// Parsing helpers

/// Epoch milliseconds → local RFC3339.
fn ts_ms_to_local(ts_ms: i64) -> String {
    DateTime::from_timestamp_millis(ts_ms)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|| format!("{ts_ms}"))
}

/// Parse Snapchat's date format: `"2023-11-14 10:00:00 UTC"` → RFC3339 UTC.
fn parse_snapchat_date(s: &str) -> String {
    // Normalize: strip " UTC" suffix, parse as NaiveDateTime, then to UTC.
    let trimmed = s.trim_end_matches(" UTC").trim();
    if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        if let Some(utc) = chrono::Utc.from_local_datetime(&naive).single() {
            return utc.with_timezone(&Local).to_rfc3339();
        }
    }
    // Fallback: return the original string if it doesn't parse.
    s.to_string()
}

/// Parse `"Latitude, Longitude: 37.77, -122.41"` → (lat, lon).
/// Returns `None` for empty strings, unparseable input, or the (0.0, 0.0)
/// sentinel that Snapchat emits for memories with no geotag.
fn parse_location(s: &str) -> Option<(f64, f64)> {
    // Strip the "Latitude, Longitude: " prefix.
    let coords = s
        .trim()
        .trim_start_matches("Latitude, Longitude:")
        .trim();
    if coords.is_empty() {
        return None;
    }
    let mut parts = coords.splitn(2, ',');
    let lat = parts.next()?.trim().parse::<f64>().ok()?;
    let lon = parts.next()?.trim().parse::<f64>().ok()?;
    // (0.0, 0.0) is Snapchat's "no geotag" sentinel — treat as absent.
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some((lat, lon))
}

/// sha256(contact | ts_key | from | content) — length-prefixed.
/// `ts_key` is the raw numeric string (epoch ms) or ISO string used as the stable key.
fn chat_guid(contact: &str, ts_key: &str, from: &str, content: &str) -> String {
    let mut h = Sha256::new();
    for part in [contact, ts_key, from, content] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}

/// sha256(date | location | url) — length-prefixed.
fn memory_guid(date: &str, location: &str, url: &str) -> String {
    let mut h = Sha256::new();
    for part in [date, location, url] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-snapchat-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run_params() -> BTreeMap<String, String> {
        [("acknowledge".to_string(), "yes".to_string())].into()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &run_params(), &mut |_| {}).unwrap()
    }

    // ------------------------------------------------------------------
    // Fixture builders

    /// Build a synthetic Snapchat My Data export ZIP (2024+ format) containing:
    /// - json/chat_history.json  (two contacts, TEXT + MEDIA messages)
    /// - json/memories_history.json  (two memories: photo + video)
    /// - json/account.json  (account info)
    ///
    /// Timestamps use real-world 13-digit epoch milliseconds matching the
    /// ISO Created strings.  e.g. "2023-11-14 20:53:20 UTC" = 1700000000000 ms.
    fn build_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-snap-zip-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // chat_history.json — flat dict {contact: [{messages}]} (2024+ format)
        // Timestamps: 1700000000000 ms = 2023-11-14 20:53:20 UTC (13 digits).
        z.start_file("json/chat_history.json", opts).unwrap();
        z.write_all(
            br#"{
  "alice": [
    {
      "From": "alice",
      "IsSender": false,
      "Created": "2023-11-14 20:53:20 UTC",
      "Created(microseconds)": 1700000000000,
      "Content": "Hey!",
      "Media Type": "TEXT",
      "Media IDs": null
    },
    {
      "From": "me",
      "IsSender": true,
      "Created": "2023-11-14 20:54:20 UTC",
      "Created(microseconds)": 1700000060000,
      "Content": "Hey back!",
      "Media Type": "TEXT",
      "Media IDs": null
    },
    {
      "From": "alice",
      "IsSender": false,
      "Created": "2023-11-14 20:55:20 UTC",
      "Created(microseconds)": 1700000120000,
      "Content": "",
      "Media Type": "MEDIA",
      "Media IDs": "snap-media-123"
    }
  ],
  "bob": [
    {
      "From": "bob",
      "IsSender": false,
      "Created": "2023-11-15 03:40:00 UTC",
      "Created(microseconds)": 1700026800000,
      "Content": "what's up",
      "Media Type": "TEXT",
      "Media IDs": null
    }
  ]
}"#,
        )
        .unwrap();

        // memories_history.json — { "Saved Media": [{...}] }
        // Uses "Download Link" (canonical field name from real exports).
        z.start_file("json/memories_history.json", opts).unwrap();
        z.write_all(
            br#"{
  "Saved Media": [
    {
      "Date": "2023-11-14 10:00:00 UTC",
      "Media Type": "Image",
      "Location": "Latitude, Longitude: 37.77, -122.41",
      "Download Link": "https://sc-cdn.snapchat.com/abc123"
    },
    {
      "Date": "2023-11-15 14:30:00 UTC",
      "Media Type": "Video",
      "Location": "",
      "Download Link": "https://sc-cdn.snapchat.com/def456"
    }
  ]
}"#,
        )
        .unwrap();

        // account.json — basic info (raw-only category)
        z.start_file("json/account.json", opts).unwrap();
        z.write_all(
            br#"{"Basic Information": {"Username": "testuser", "Creation Date": "2020-01-01"}}"#,
        )
        .unwrap();

        z.finish().unwrap();
        path
    }

    /// Build a pre-2024 format export ZIP.
    /// Schema: { "Received Chat History": [...], "Sent Chat History": [...] }
    /// Body in "Text" not "Content", no numeric timestamp, From/To fields.
    fn build_pre2024_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-snap-pre2024-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        z.start_file("json/chat_history.json", opts).unwrap();
        z.write_all(
            br#"{
  "Received Chat History": [
    {
      "From": "charlie",
      "To": "me",
      "Created": "2022-05-01 12:00:00 UTC",
      "Text": "Hello from pre-2024",
      "Media Type": "TEXT"
    },
    {
      "From": "charlie",
      "To": "me",
      "Created": "2022-05-01 12:02:00 UTC",
      "Text": "",
      "Media Type": "MEDIA"
    }
  ],
  "Sent Chat History": [
    {
      "From": "me",
      "To": "charlie",
      "Created": "2022-05-01 12:01:00 UTC",
      "Text": "Hey charlie!",
      "Media Type": "TEXT"
    }
  ]
}"#,
        )
        .unwrap();

        z.start_file("json/account.json", opts).unwrap();
        z.write_all(br#"{"Basic Information": {"Username": "olduser"}}"#).unwrap();

        z.finish().unwrap();
        path
    }

    // ------------------------------------------------------------------
    // Tests

    #[test]
    fn imports_chat_messages_from_two_conversations() {
        let v = temp_vault("chats");
        let zip = build_zip("chats");
        let out = run(&v, &zip);

        // 3 messages from alice + 1 from bob = 4 total.
        assert_eq!(
            out.counts.get("chat_messages"),
            Some(&4),
            "{}",
            out.headline
        );
        assert_eq!(
            out.counts.get("conversations"),
            Some(&2),
            "{}",
            out.headline
        );

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn imports_memories_with_location() {
        let v = temp_vault("memories");
        let zip = build_zip("memories");
        let out = run(&v, &zip);

        assert_eq!(
            out.counts.get("memories"),
            Some(&2),
            "{}",
            out.headline
        );

        // Check photo row has lat/lon.
        let stream = v.stream(PHOTOS_DIR, Partition::Month);
        let parts = stream.partitions().unwrap();
        let mut all_photos: Vec<Photo> = Vec::new();
        for key in parts {
            all_photos.extend(stream.read::<Photo>(&key).unwrap());
        }
        assert_eq!(all_photos.len(), 2);

        let photo = all_photos.iter().find(|p| p.kind == "photo").unwrap();
        assert_eq!(photo.lat, Some(37.77));
        assert_eq!(photo.lon, Some(-122.41));
        assert_eq!(photo.extra.get("download_url").and_then(Value::as_str), Some("https://sc-cdn.snapchat.com/abc123"));

        let video = all_photos.iter().find(|p| p.kind == "video").unwrap();
        assert!(video.lat.is_none(), "no lat when location is empty");

        let _ = fs::remove_file(build_zip("memories-check"));
    }

    #[test]
    fn from_me_set_correctly() {
        let v = temp_vault("fromme");
        let zip = build_zip("fromme");
        run(&v, &zip);

        // Read correspondence month.
        let month = v.root().join("correspondence/snapchat/2023-11.jsonl");
        assert!(month.exists(), "month file must exist");
        let content = fs::read_to_string(&month).unwrap();
        let rows: Vec<Message> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let mine = rows
            .iter()
            .find(|m| m.chat == "alice" && m.text == "Hey back!")
            .unwrap();
        assert!(mine.from_me, "IsSender=true → from_me");
        assert!(mine.sender.is_empty(), "sender empty when from_me");

        let theirs = rows
            .iter()
            .find(|m| m.chat == "alice" && m.text == "Hey!")
            .unwrap();
        assert!(!theirs.from_me);
        assert_eq!(theirs.sender, "alice");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn media_message_has_attachment_metadata_only() {
        let v = temp_vault("media");
        let zip = build_zip("media");
        run(&v, &zip);

        let month = v.root().join("correspondence/snapchat/2023-11.jsonl");
        let content = fs::read_to_string(&month).unwrap();
        let rows: Vec<Message> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let media_msg = rows
            .iter()
            .find(|m| m.chat == "alice" && !m.attachments.is_empty())
            .expect("media message not found");
        assert_eq!(media_msg.attachments[0].name, "snap-media-123");
        assert_eq!(media_msg.attachments[0].mime, "media");
        assert_eq!(media_msg.attachments[0].bytes, 0, "metadata only — no bytes");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reimport_is_noop() {
        let v = temp_vault("reimport");
        let zip = build_zip("reimport");

        let first = run(&v, &zip);
        let first_chats = *first.counts.get("chat_messages").unwrap_or(&0);
        let first_memories = *first.counts.get("memories").unwrap_or(&0);

        let second = run(&v, &zip);
        assert_eq!(
            second.counts.get("chat_messages"),
            Some(&0),
            "no new chats on re-import"
        );
        assert_eq!(
            second.counts.get("memories"),
            Some(&0),
            "no new memories on re-import"
        );
        assert!(
            second.counts.get("duplicates").unwrap_or(&0)
                >= &(first_chats + first_memories),
            "duplicates count covers all first-import rows"
        );

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn raw_files_written_to_social_folder() {
        let v = temp_vault("raw");
        let zip = build_zip("raw");
        let out = run(&v, &zip);

        assert!(
            out.counts.get("raw_files").copied().unwrap_or(0) >= 3,
            "at least 3 raw JSON files written (chat_history, memories_history, account)"
        );

        // Check that a raw file exists somewhere under social/snapchat/raw/.
        let raw_root = v.root().join("social/snapchat/raw");
        assert!(raw_root.exists(), "raw root must exist");
        let has_chat = walkdir_has(&raw_root, "chat_history.json");
        assert!(has_chat, "chat_history.json in raw layer");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn missing_category_is_graceful() {
        // A ZIP with only account.json — no chats, no memories.
        let path = std::env::temp_dir().join(format!(
            "trove-snap-sparse-{}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("json/account.json", opts).unwrap();
        z.write_all(br#"{"Basic Information":{"Username":"user","Creation Date":"2020"}}"#)
            .unwrap();
        z.finish().unwrap();

        let v = temp_vault("sparse");
        let out = run(&v, &path);
        assert_eq!(out.counts.get("chat_messages"), Some(&0));
        assert_eq!(out.counts.get("memories"), Some(&0));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn date_parse_utc_format() {
        let ts = parse_snapchat_date("2023-11-14 10:00:00 UTC");
        // The date is parsed as UTC then converted to local time, so the date
        // component may shift when local time is behind UTC (e.g. PST = UTC-8).
        // The critical test: it must parse to a valid RFC3339 string (not the raw input).
        assert!(
            ts.contains('T') && (ts.contains('+') || ts.contains('-')),
            "parsed to RFC3339: {ts}"
        );
        // The input should not come back verbatim (that would mean parsing failed).
        assert_ne!(ts, "2023-11-14 10:00:00 UTC", "must parse, not pass through: {ts}");
    }

    #[test]
    fn location_parse_valid_and_empty() {
        let (lat, lon) = parse_location("Latitude, Longitude: 37.77, -122.41").unwrap();
        assert!((lat - 37.77).abs() < 1e-6);
        assert!((lon - (-122.41)).abs() < 1e-6);

        assert!(parse_location("").is_none());
        assert!(parse_location("Latitude, Longitude: ").is_none());
    }

    #[test]
    fn location_parse_null_island_sentinel_is_none() {
        // Snapchat emits (0.0, 0.0) for memories with no geotag — treat as absent.
        assert!(parse_location("Latitude, Longitude: 0.0, 0.0").is_none());
        assert!(parse_location("Latitude, Longitude: 0, 0").is_none());
    }

    #[test]
    fn chat_guid_injective() {
        // Length-prefix prevents boundary collisions.
        let g1 = chat_guid("a|b", "0", "c", "");
        let g2 = chat_guid("a", "0", "b|c", "");
        assert_ne!(g1, g2, "component-boundary collision");
        // Same inputs → same guid.
        assert_eq!(
            chat_guid("alice", "1700000000000", "alice", "hey"),
            chat_guid("alice", "1700000000000", "alice", "hey")
        );
        assert_eq!(chat_guid("a", "1", "b", "c").len(), 64);
    }

    #[test]
    fn timestamp_is_milliseconds_not_microseconds() {
        // 1700000000000 ms = 2023-11-14 20:53:20 UTC.
        // If incorrectly divided by 1000 → 1700000000 ms = 2023-11-14 (still 2023, but wrong)
        // But with the original 16-digit fabricated value ÷ 1000 → 1970.
        // This test pins the real 13-digit ms value to the correct calendar month.
        let ts = ts_ms_to_local(1700000000000_i64);
        // Must be in 2023-11, not 1970.
        assert!(
            ts.starts_with("2023-11"),
            "1700000000000 ms must parse to 2023-11, got: {ts}"
        );
    }

    #[test]
    fn pre2024_format_imports_chats() {
        let v = temp_vault("pre2024");
        let zip = build_pre2024_zip("pre2024");
        let out = run(&v, &zip);

        // 2 received + 1 sent = 3 messages from 1 conversation (charlie).
        assert_eq!(
            out.counts.get("chat_messages"),
            Some(&3),
            "pre-2024 format: {}",
            out.headline
        );
        assert_eq!(
            out.counts.get("conversations"),
            Some(&1),
            "pre-2024 format: {}",
            out.headline
        );

        // Verify from_me is set correctly for sent messages.
        let month = v.root().join("correspondence/snapchat/2022-05.jsonl");
        assert!(month.exists(), "pre-2024 month file must exist at correspondence/snapchat/2022-05.jsonl");
        let content = fs::read_to_string(&month).unwrap();
        let rows: Vec<Message> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let sent = rows.iter().find(|m| m.text == "Hey charlie!").unwrap();
        assert!(sent.from_me, "Sent Chat History → from_me=true");

        let received = rows.iter().find(|m| m.text == "Hello from pre-2024").unwrap();
        assert!(!received.from_me, "Received Chat History → from_me=false");
        assert_eq!(received.sender, "charlie");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn pre2024_reimport_is_noop() {
        let v = temp_vault("pre2024-reimport");
        let zip = build_pre2024_zip("pre2024-reimport");

        let first = run(&v, &zip);
        let first_chats = *first.counts.get("chat_messages").unwrap_or(&0);
        assert!(first_chats > 0, "first import must have chats");

        let second = run(&v, &zip);
        assert_eq!(
            second.counts.get("chat_messages"),
            Some(&0),
            "no new chats on re-import of pre-2024 export"
        );

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn memories_download_link_field_accepted() {
        // Verify that "Download Link" (canonical) populates download_url in extra.
        let v = temp_vault("dl-link");
        let zip = build_zip("dl-link");
        run(&v, &zip);

        let stream = v.stream(PHOTOS_DIR, Partition::Month);
        let parts = stream.partitions().unwrap();
        let mut all_photos: Vec<Photo> = Vec::new();
        for key in parts {
            all_photos.extend(stream.read::<Photo>(&key).unwrap());
        }
        assert!(!all_photos.is_empty());
        let has_url = all_photos
            .iter()
            .any(|p| p.extra.get("download_url").is_some());
        assert!(has_url, "download_url must be set from 'Download Link' field");
    }

    #[test]
    fn old_message_lines_still_deserialize() {
        let line = r#"{"ts":"2023-11-14T12:00:00-07:00","source":"snapchat","chat":"alice","from_me":false,"kind":"message","text":"hey"}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hey");
        assert_eq!(m.source, "snapchat");
    }

    // Helper: walk a dir tree looking for a filename.
    fn walkdir_has(dir: &std::path::Path, target: &str) -> bool {
        let Ok(rd) = fs::read_dir(dir) else { return false };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if walkdir_has(&p, target) {
                    return true;
                }
            } else if p.file_name().and_then(|n| n.to_str()) == Some(target) {
                return true;
            }
        }
        false
    }
}
