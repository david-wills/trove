//! Google Chat — import of the Takeout JSON export (spaces, DMs, and legacy
//! Hangouts). The Chat REST API is bot/tenant-oriented and unsuitable for
//! personal history.
//!
//! # Takeout layout
//!
//! ```text
//! Takeout/
//!   Google Chat/
//!     Groups/
//!       Space_<id>/              Space (group chat)
//!         group_info.json          {"name": "Space Name", "members": [...]}
//!         messages.json            {"messages": [...]}
//!       DM_<id>/                 Direct message conversation
//!         group_info.json
//!         messages.json
//!   Hangouts/
//!     Hangouts.json              legacy format — {"conversations": [...]}
//! ```
//!
//! Note: All conversations (both Spaces and DMs) live under `Groups/`. There
//! is no separate `DMs/` top-level folder. `messages.json` is placed directly
//! in the conversation folder — there is no `Messages/` subfolder.
//!
//! A whole-Takeout ZIP drop is handled by inspecting the paths inside; a
//! Chat-only sub-folder drop works the same way.
//!
//! # Message format (Google Chat native)
//!
//! `messages.json` contains an object with a `"messages"` array:
//! ```json
//! {"messages": [
//!   {
//!     "creator": {"name": "Jane Doe", "email": "jane@example.com"},
//!     "created_date": "Thursday, 8 August 2024 at 12:04:08 UTC",
//!     "text": "Hello everyone!",
//!     "topic_id": "...",
//!     "message_id": "...",
//!     "attached_files": [{"original_name": "photo.png", "export_name": "..."}],
//!     "annotations": []
//!   }
//! ]}
//! ```
//! Note: `created_date` is a human-readable string with an optional " at "
//! separator and potentially single-digit days. RFC3339 is also handled for
//! newer exports.
//!
//! `group_info.json`:
//! ```json
//! {"name": "My Space", "members": [{"email": "a@b.com", "name": "Alice"}]}
//! ```
//!
//! # Hangouts.json format (legacy)
//!
//! Top-level key may be `"conversations"` or `"conversation_state"` depending
//! on the export variant. Event list may be `"events"` or `"event"` (singular).
//! Sender id field may be `"gaia_id"` or `"chat_id"`. Both variants are
//! accepted. Timestamp is in **microseconds since epoch** as a decimal string;
//! message body under `chat_message.message_content.segment[].text`.
//! Legacy Hangouts rows carry `"hangouts-legacy"` in their `labels` array.
//!
//! # Guid strategy
//!
//! Chat native: `message_id` from the JSON. Falls back to a stable hash of
//! (space_id, created_date, creator_email) if `message_id` is absent.
//! Hangouts: `{conv_id}/{event_id}` from the conversation/event objects.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/google-chat.md.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Integration definition

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/google-chat"))
}

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[crate::registry::ImportParam {
        key: "me",
        label: "Your Google account email",
        placeholder: "you@gmail.com (optional — marks your messages as sent)",
        required: false,
    }],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-chat",
        name: "Google Chat",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Google Chat history from a Google Takeout export ZIP, \
                      covering spaces, direct messages, and legacy Hangouts conversations. \
                      Re-runnable; messages deduplicate on their stable id.",
        domain: "correspondence",
        vault_path: "correspondence/google-chat/",
        toggleable: false,
        setup: &[
            "Google Takeout (takeout.google.com) → select Google Chat → create and download the export ZIP.",
            "Drop the ZIP here. A whole-Takeout ZIP works too — only the Chat and Hangouts subtrees are imported.",
        ],
        caveats: "Takeout is the only supported path; the Chat REST API does not expose \
                  personal message history. Message bodies are stored verbatim — use a \
                  dedicated vault only you control.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Google Chat native message shapes

/// One member record in group_info.json. Fields are deserialized for full
/// fidelity; direct field reads are a read-time concern, not needed at import.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
struct GroupMember {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
}

/// group_info.json
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
struct GroupInfo {
    #[serde(default)]
    name: String,
    #[serde(default)]
    members: Vec<GroupMember>,
}

/// One entry in the `attached_files` array of a Chat message.
#[derive(Debug, Default, Deserialize)]
struct ChatAttachment {
    #[serde(default)]
    original_name: String,
    #[serde(default)]
    export_name: String,
}

/// One message in a `Messages/*.json` array.
#[derive(Debug, Default, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    creator: ChatCreator,
    /// Either a human-readable Google date string
    /// ("Monday, 10 May 2021, 10:23:00 UTC") or RFC3339 in newer exports.
    #[serde(default)]
    created_date: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    topic_id: String,
    #[serde(default)]
    attached_files: Vec<ChatAttachment>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatCreator {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
}

// ---------------------------------------------------------------------------
// Hangouts.json shapes
//
// Two export variants are known in the wild:
//   Variant A: top-level "conversations", event list "events", id field "gaia_id"
//   Variant B: top-level "conversation_state", event list "event" (singular),
//              id field "chat_id"
// Both are accepted below via serde aliases.

/// Top level of Hangouts.json. Accepts both known export variants.
#[derive(Debug, Default, Deserialize)]
struct HangoutsRoot {
    /// Variant A: "conversations" key.
    #[serde(default)]
    conversations: Vec<HangoutsConversation>,
    /// Variant B: "conversation_state" key — merged with `conversations` at
    /// parse time in the importer.
    #[serde(default)]
    conversation_state: Vec<HangoutsConversation>,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsConversation {
    #[serde(default)]
    conversation: HangoutsConvMeta,
    /// Variant A: "events".
    #[serde(default)]
    events: Vec<HangoutsEvent>,
    /// Variant B: "event" (singular).
    #[serde(default)]
    event: Vec<HangoutsEvent>,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsConvMeta {
    #[serde(default)]
    id: HangoutsConvId,
    /// Human-visible name for a group; empty for 1:1 conversations.
    #[serde(default)]
    name: String,
    /// Participant data — used to resolve gaia_id/chat_id → display name.
    #[serde(default)]
    participant_data: Vec<HangoutsParticipant>,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsConvId {
    #[serde(default)]
    id: String,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsParticipant {
    #[serde(default)]
    id: HangoutsGaiaId,
    #[serde(default)]
    fallback_name: String,
}

/// Sender/participant id — accepts both "gaia_id" (variant A) and "chat_id"
/// (variant B). The first non-empty value is used.
#[derive(Debug, Default, Deserialize)]
struct HangoutsGaiaId {
    /// Variant A.
    #[serde(default)]
    gaia_id: String,
    /// Variant B.
    #[serde(default)]
    chat_id: String,
}

impl HangoutsGaiaId {
    /// Returns the first non-empty id regardless of which field carried it.
    fn id(&self) -> &str {
        if !self.gaia_id.is_empty() {
            &self.gaia_id
        } else {
            &self.chat_id
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
struct HangoutsEvent {
    #[serde(default)]
    event_id: String,
    #[serde(default)]
    sender_id: HangoutsGaiaId,
    /// Decimal string, microseconds since Unix epoch.
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    chat_message: Option<HangoutsMsg>,
    /// Deserialized to distinguish call/VOIP events from chat messages at
    /// full fidelity — not used at import time (None = no chat content).
    #[serde(default)]
    hangout_event: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsMsg {
    #[serde(default)]
    message_content: HangoutsMsgContent,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsMsgContent {
    #[serde(default)]
    segment: Vec<HangoutsSegment>,
    #[serde(default)]
    attachment: Vec<HangoutsAttachment>,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsSegment {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsAttachment {
    #[serde(default)]
    embed_item: HangoutsEmbedItem,
}

#[derive(Debug, Default, Deserialize)]
struct HangoutsEmbedItem {
    /// Plus-photo, link, etc. The name string (e.g. "PLUS_PHOTO").
    #[serde(rename = "type", default)]
    kind: Vec<String>,
}

// ---------------------------------------------------------------------------
// Import stats

#[derive(Default)]
struct Stats {
    messages: u64,
    duplicates: u64,
    spaces: u32,
    hangouts: u64,
}

// ---------------------------------------------------------------------------
// Timestamp parsers

/// RFC3339 or "Day, D Mon YYYY at HH:MM:SS UTC" → local RFC3339. `None` when
/// unparseable.
///
/// Real Google Chat exports use single-digit days ("Thursday, 8 August 2024
/// at 12:04:08 UTC") as well as two-digit days. The format uses full month
/// names and an " at " separator (older exports may omit the " at "). Both
/// forms and either day width are handled.
fn chat_date_to_local(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // RFC3339 / ISO 8601 (newer exports).
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Local).to_rfc3339());
    }
    // "Thursday, 8 August 2024 at 12:04:08 UTC" — Google's human-readable format.
    // Strip the weekday prefix ("Thursday, ") before parsing.
    let stripped = if let Some(pos) = s.find(", ") {
        &s[pos + 2..]
    } else {
        s
    };
    // Normalize: remove commas and " at " separator.
    // Produces: "8 August 2024 12:04:08 UTC" (single-digit day possible)
    let normalized = stripped.replace(", ", " ").replace(" at ", " ");
    // Trim trailing " UTC" for the NaiveDateTime parse, then pad day if needed.
    let without_tz = normalized.trim_end_matches(" UTC").trim();
    // Zero-pad a leading single-digit day so chrono's %d accepts it.
    // E.g. "8 August 2024 12:04:08" → "08 August 2024 12:04:08"
    let padded;
    let to_parse = {
        let first_space = without_tz.find(' ').unwrap_or(without_tz.len());
        if first_space == 1 {
            padded = format!("0{without_tz}");
            padded.as_str()
        } else {
            without_tz
        }
    };
    // Try "%d %B %Y %H:%M:%S" (full month name)
    if let Ok(naive) = NaiveDateTime::parse_from_str(to_parse, "%d %B %Y %H:%M:%S") {
        let utc = Utc.from_utc_datetime(&naive);
        return Some(utc.with_timezone(&Local).to_rfc3339());
    }
    // Fallback: abbreviated month name (some locales / older exports use "May" etc.)
    if let Ok(naive) = NaiveDateTime::parse_from_str(to_parse, "%d %b %Y %H:%M:%S") {
        let utc = Utc.from_utc_datetime(&naive);
        return Some(utc.with_timezone(&Local).to_rfc3339());
    }
    None
}

/// Hangouts microsecond-epoch decimal string → local RFC3339.
fn hangouts_ts_to_local(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let micros: i64 = s.parse().ok()?;
    let secs = micros / 1_000_000;
    let nanos = ((micros % 1_000_000) * 1000) as u32;
    let t = DateTime::from_timestamp(secs, nanos)?;
    Some(t.with_timezone(&Local).to_rfc3339())
}

/// Stable fallback guid: hex-truncated SHA-256 of the given string.
fn stable_hash_guid(input: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    input.hash(&mut h);
    format!("hash-{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// The importer

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let me = params
        .get("me")
        .map(String::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_lowercase);

    let mut seen = vault.correspondence_guids("google-chat")?;
    let mut messages: Vec<Message> = Vec::new();
    let mut stats = Stats::default();

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;

    // Collect all entry names first so we can multi-pass without borrow issues.
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    // -----------------------------------------------------------------------
    // Pass 1: collect group_info.json entries so we can attach chat names.
    // Key: the conversation folder path → GroupInfo.
    use std::collections::HashMap;
    let mut group_infos: HashMap<String, GroupInfo> = HashMap::new();

    for name in &names {
        if !name.ends_with("group_info.json") {
            continue;
        }
        // Path: "Takeout/Google Chat/Groups/<id>/group_info.json"
        //   or: "Takeout/Google Chat/DMs/<id>/group_info.json"
        //   or: "Google Chat/Groups/<id>/group_info.json"
        // The folder key is the parent directory path.
        let conv_folder = name.trim_end_matches("group_info.json").trim_end_matches('/').to_string();
        if conv_folder.is_empty() {
            continue;
        }
        let mut body = String::new();
        if zip.by_name(name).map(|mut e| e.read_to_string(&mut body)).is_ok() {
            if let Ok(info) = serde_json::from_str::<GroupInfo>(&body) {
                group_infos.insert(conv_folder, info);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pass 2: Chat native Messages/*.json files.
    for name in &names {
        if !is_chat_messages_file(name) {
            continue;
        }
        // Determine conversation folder — everything up to (but not including)
        // the "Messages/" segment.
        let conv_folder = chat_conv_folder(name);
        let chat_id = conv_folder
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .to_string();
        let chat_name = group_infos
            .get(&conv_folder)
            .map(|g| g.name.clone())
            .unwrap_or_default();

        let mut body = String::new();
        if zip.by_name(name).map(|mut e| e.read_to_string(&mut body)).is_err() {
            continue;
        }

        // Real messages.json shape: {"messages": [...]}
        // Older/fallback: bare JSON array.
        let msgs: Vec<ChatMessage> = {
            let v: Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v {
                Value::Array(arr) => arr
                    .into_iter()
                    .filter_map(|x| serde_json::from_value(x).ok())
                    .collect(),
                Value::Object(ref obj) => {
                    // Standard shape: {"messages": [...]}
                    if let Some(Value::Array(arr)) = obj.get("messages") {
                        arr.iter()
                            .cloned()
                            .filter_map(|x| serde_json::from_value(x).ok())
                            .collect()
                    } else {
                        // Unknown object shape — skip rather than misparse.
                        continue;
                    }
                }
                _ => continue,
            }
        };

        if !msgs.is_empty() {
            stats.spaces += 1;
        }

        for cm in msgs {
            let Some(ts) = chat_date_to_local(&cm.created_date) else { continue };

            // Guid: message_id if available, else stable hash.
            let guid = if !cm.message_id.is_empty() {
                cm.message_id.clone()
            } else {
                stable_hash_guid(&format!("{}/{}/{}", chat_id, cm.created_date, cm.creator.email))
            };

            if !seen.insert(guid.clone()) {
                stats.duplicates += 1;
                continue;
            }

            let creator_email_lc = cm.creator.email.to_lowercase();
            let from_me = me.as_ref().is_some_and(|m| *m == creator_email_lc);

            let mut m = Message::new("google-chat", ts);
            m.guid = guid;
            m.chat = chat_id.clone();
            m.chat_name = chat_name.clone();
            m.from_me = from_me;
            if !from_me {
                m.sender = cm.creator.email.clone();
                m.sender_name = cm.creator.name.clone();
                if m.sender_name == m.sender {
                    m.sender_name = String::new();
                }
            }
            m.text = cm.text.clone();
            if !cm.topic_id.is_empty() && cm.topic_id != cm.message_id {
                m.reply_to = cm.topic_id.clone();
            }
            m.attachments = cm
                .attached_files
                .iter()
                .map(|f| AttachmentMeta {
                    name: if !f.original_name.is_empty() {
                        f.original_name.clone()
                    } else {
                        f.export_name.clone()
                    },
                    mime: String::new(),
                    bytes: 0,
                })
                .collect();
            if m.text.is_empty() && m.attachments.is_empty() {
                seen.remove(&m.guid);
                continue;
            }
            messages.push(m);
            stats.messages += 1;

            if messages.len() >= 2000 {
                vault.append_messages(&messages)?;
                messages.clear();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pass 3: Hangouts.json (legacy).
    for name in &names {
        if !name.ends_with("Hangouts/Hangouts.json") && !name.ends_with("Hangouts.json") {
            continue;
        }
        let mut body = String::new();
        if zip.by_name(name).map(|mut e| e.read_to_string(&mut body)).is_err() {
            continue;
        }
        let root: HangoutsRoot = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Merge both variant keys: "conversations" (variant A) and
        // "conversation_state" (variant B) into a single iterator.
        let all_convs = root.conversations.into_iter().chain(root.conversation_state);
        for conv in all_convs {
            let conv_id = conv.conversation.id.id.clone();
            if conv_id.is_empty() {
                continue;
            }
            let chat_name = conv.conversation.name.clone();

            // Build id → display name map for this conversation.
            // Accepts both gaia_id (variant A) and chat_id (variant B).
            let name_map: HashMap<String, String> = conv
                .conversation
                .participant_data
                .iter()
                .filter(|p| !p.id.id().is_empty())
                .map(|p| (p.id.id().to_string(), p.fallback_name.clone()))
                .collect();

            // Merge both event list keys: "events" (variant A) and
            // "event" (singular, variant B).
            let all_events = conv.events.into_iter().chain(conv.event);
            for ev in all_events {
                let event_id = ev.event_id.clone();
                if event_id.is_empty() {
                    continue;
                }
                let Some(ts) = hangouts_ts_to_local(&ev.timestamp) else { continue };

                // Skip non-chat events (call events, etc.).
                let chat_msg = match &ev.chat_message {
                    Some(m) => m,
                    None => continue,
                };

                // Reconstruct text from segment array.
                let text: String = chat_msg
                    .message_content
                    .segment
                    .iter()
                    .filter(|s| s.kind == "TEXT" || s.kind.is_empty())
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("");

                let attachments: Vec<AttachmentMeta> = chat_msg
                    .message_content
                    .attachment
                    .iter()
                    .map(|a| AttachmentMeta {
                        name: a.embed_item.kind.first().cloned().unwrap_or_default(),
                        mime: String::new(),
                        bytes: 0,
                    })
                    .filter(|a| !a.name.is_empty())
                    .collect();

                if text.is_empty() && attachments.is_empty() {
                    continue;
                }

                let guid = format!("{conv_id}/{event_id}");
                if !seen.insert(guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }

                let sender_id = ev.sender_id.id().to_string();
                let sender_name =
                    name_map.get(&sender_id).cloned().unwrap_or_else(|| sender_id.clone());

                let mut m = Message::new("google-chat", ts);
                m.guid = guid;
                m.chat = conv_id.clone();
                m.chat_name = chat_name.clone();
                m.sender = sender_id;
                m.sender_name = sender_name;
                m.text = text;
                m.attachments = attachments;
                // Mark as legacy Hangouts in labels so read-time queries can
                // filter. The `service` field is reserved for the account
                // address per the correspondence contract.
                m.labels = vec!["hangouts-legacy".into()];

                messages.push(m);
                stats.hangouts += 1;

                if messages.len() >= 2000 {
                    vault.append_messages(&messages)?;
                    messages.clear();
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    vault.append_messages(&messages)?;
    progress(ImportProgress { records: stats.messages + stats.hangouts, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{} Chat messages and {} Hangouts messages imported from {} conversations, {} duplicates skipped",
            stats.messages,
            stats.hangouts,
            stats.spaces,
            stats.duplicates,
        ),
        counts: [
            ("messages", stats.messages),
            ("hangouts", stats.hangouts),
            ("conversations", stats.spaces as u64),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Path helpers

/// True when the zip entry is a Chat native messages file.
///
/// Real Google Takeout layout: `messages.json` sits **directly** in the
/// conversation folder under `Google Chat/Groups/<id>/messages.json`. There is
/// no `Messages/` subfolder and no numbered `messages_NNN.json` files. All
/// conversations (both `Space_*` group chats and `DM_*` direct messages) live
/// under `Groups/` — there is no separate top-level `DMs/` folder.
///
/// Accepted paths (case-insensitive match on filename):
///   `...Google Chat/Groups/<id>/messages.json`
fn is_chat_messages_file(name: &str) -> bool {
    // Must end with /messages.json (case-insensitive).
    if !name.to_ascii_lowercase().ends_with("/messages.json") {
        return false;
    }
    // Must be under a Google Chat/Groups/ subtree.
    // Accept both "Google Chat/Groups/" and "google chat/groups/" (ZIP paths
    // from different OS exports occasionally vary in case).
    let name_lc = name.to_ascii_lowercase();
    name_lc.contains("google chat/groups/")
}

/// The conversation folder path — the parent directory of `messages.json`.
fn chat_conv_folder(name: &str) -> String {
    match name.rfind('/') {
        Some(pos) => name[..pos].to_string(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;

    fn temp_vault(tag: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-gchat-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &std::path::PathBuf, me: Option<&str>) -> ImportOutcome {
        let mut params = BTreeMap::new();
        if let Some(m) = me {
            params.insert("me".to_string(), m.to_string());
        }
        (IMPORT.run)(v, path, &params, &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Timestamp parser tests

    #[test]
    fn chat_date_parses() {
        // Human-readable format with two-digit day (older exports, comma separator).
        let t =
            chat_date_to_local("Monday, 10 May 2021, 10:23:00 UTC").unwrap();
        assert!(t.starts_with("2021-05-10"), "parsed two-digit day: {t}");

        // Human-readable with " at " separator (current export format).
        let t_at =
            chat_date_to_local("Monday, 10 May 2021 at 10:23:00 UTC").unwrap();
        assert!(t_at.starts_with("2021-05-10"), "parsed at-format: {t_at}");

        // Single-digit day — the critical fix (1-9 of any month).
        let t_single =
            chat_date_to_local("Thursday, 8 August 2024 at 12:04:08 UTC").unwrap();
        assert!(t_single.starts_with("2024-08-08"), "single-digit day: {t_single}");

        // RFC3339 from newer exports.
        let t2 = chat_date_to_local("2021-05-10T10:23:00.000000Z").unwrap();
        assert!(t2.starts_with("2021-05-10"), "rfc3339: {t2}");

        // Empty → None.
        assert!(chat_date_to_local("").is_none());
    }

    #[test]
    fn hangouts_ts_parses() {
        // 1_620_645_780_000_000 µs = 2021-05-10T10:23:00Z
        let t = hangouts_ts_to_local("1620645780000000").unwrap();
        assert!(t.starts_with("2021-05-10"), "hangouts ts: {t}");
        assert!(hangouts_ts_to_local("").is_none());
        assert!(hangouts_ts_to_local("junk").is_none());
    }

    // -----------------------------------------------------------------------
    // Minimal Chat-native ZIP fixture

    /// Build a ZIP with one Space + one DM (both under Groups/) + Hangouts.json.
    ///
    /// Uses the REAL Google Takeout layout:
    ///   - messages.json directly in the conversation folder (no Messages/ subfolder)
    ///   - {"messages": [...]} wrapper object
    ///   - DMs are under Groups/DM_* (NOT a separate DMs/ top-level folder)
    ///   - Single-digit day in one created_date to exercise that parser path
    fn fake_gchat_zip(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-gchat-fake-{}-{tag}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let f = fs::File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();

        // Space group_info
        z.start_file(
            "Takeout/Google Chat/Groups/space123/group_info.json",
            opts,
        )
        .unwrap();
        z.write_all(
            br#"{"name":"Engineering","members":[{"email":"alice@example.com","name":"Alice"},{"email":"bob@example.com","name":"Bob"}]}"#,
        )
        .unwrap();

        // Space messages — real shape: {"messages": [...]}
        // Single-digit day (8 May) in first message to exercise that parser path.
        z.start_file(
            "Takeout/Google Chat/Groups/space123/messages.json",
            opts,
        )
        .unwrap();
        z.write_all(
            br#"{"messages":[
              {"message_id":"msg001","creator":{"name":"Alice Smith","email":"alice@example.com"},"created_date":"Monday, 8 May 2021 at 10:23:00 UTC","text":"Hello everyone!","attached_files":[]},
              {"message_id":"msg002","creator":{"name":"Bob Jones","email":"bob@example.com"},"created_date":"Monday, 10 May 2021 at 10:25:00 UTC","text":"Hey Alice!","topic_id":"msg001","attached_files":[{"original_name":"screenshot.png","export_name":"exp_001.png"}]}
            ]}"#,
        )
        .unwrap();

        // DM group_info — DMs are under Groups/DM_* (NOT a separate DMs/ folder)
        z.start_file(
            "Takeout/Google Chat/Groups/DM_dm456/group_info.json",
            opts,
        )
        .unwrap();
        z.write_all(
            br#"{"name":"","members":[{"email":"alice@example.com","name":"Alice"},{"email":"carol@example.com","name":"Carol"}]}"#,
        )
        .unwrap();

        // DM messages — real shape: {"messages": [...]}
        z.start_file(
            "Takeout/Google Chat/Groups/DM_dm456/messages.json",
            opts,
        )
        .unwrap();
        z.write_all(
            br#"{"messages":[
              {"message_id":"dm001","creator":{"name":"Carol Evans","email":"carol@example.com"},"created_date":"2021-05-10T11:00:00.000000Z","text":"Hi Alice, private message!","attached_files":[]}
            ]}"#,
        )
        .unwrap();

        // Hangouts.json (legacy) — variant A: "conversations", "events", "gaia_id"
        z.start_file("Takeout/Hangouts/Hangouts.json", opts).unwrap();
        z.write_all(
            br#"{"conversations":[
              {"conversation":{"id":{"id":"conv_abc"},"name":"Old Friends","participant_data":[
                {"id":{"gaia_id":"gaia001"},"fallback_name":"Alice Old"},
                {"id":{"gaia_id":"gaia002"},"fallback_name":"Bob Old"}
              ]},
              "events":[
                {"event_id":"ev001","sender_id":{"gaia_id":"gaia001"},"timestamp":"1620645780000000",
                 "chat_message":{"message_content":{"segment":[{"type":"TEXT","text":"Legacy message!"}],"attachment":[]}}},
                {"event_id":"ev002","sender_id":{"gaia_id":"gaia002"},"timestamp":"1620645900000000",
                 "chat_message":{"message_content":{"segment":[{"type":"TEXT","text":"Reply here"}],"attachment":[]}}}
              ]}
            ]}"#,
        )
        .unwrap();

        z.finish().unwrap();
        path
    }

    #[test]
    fn import_spaces_dms_and_hangouts() {
        let v = temp_vault("import");
        let zip_path = fake_gchat_zip("import");

        let out = run(&v, &zip_path, Some("alice@example.com"));

        // 2 space messages + 1 DM message = 3 Chat native.
        assert_eq!(out.counts.get("messages"), Some(&3), "chat messages: {out:?}");
        // 2 Hangouts events.
        assert_eq!(out.counts.get("hangouts"), Some(&2), "hangouts: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // msg001 is on 2021-05-08 (single-digit day "8 May"), msg002 on 2021-05-10.
        // DM and Hangouts are on 2021-05-10.
        let day8 = v.correspondence_timeline("2021-05-08").unwrap();
        assert_eq!(day8.len(), 1, "only msg001 on 2021-05-08: {:?}", day8);

        let day10 = v.correspondence_timeline("2021-05-10").unwrap();
        // msg002 + dm001 + 2 Hangouts = 4 total on 2021-05-10.
        assert_eq!(day10.len(), 4, "total messages on 2021-05-10: {:?}", day10);

        // Alice's message is from_me (single-digit day: 8 May).
        let alice_msg = day8.iter().find(|m| m.guid == "msg001").unwrap();
        assert!(alice_msg.from_me, "alice's message should be from_me");
        assert_eq!(alice_msg.sender, "", "sender empty when from_me");
        assert_eq!(alice_msg.chat, "space123");
        assert_eq!(alice_msg.chat_name, "Engineering");
        assert_eq!(alice_msg.text, "Hello everyone!");

        // Bob's reply has an attachment and thread pointer (10 May).
        let bob_msg = day10.iter().find(|m| m.guid == "msg002").unwrap();
        assert!(!bob_msg.from_me);
        assert_eq!(bob_msg.sender, "bob@example.com");
        assert_eq!(bob_msg.sender_name, "Bob Jones");
        assert_eq!(bob_msg.reply_to, "msg001");
        assert_eq!(bob_msg.attachments.len(), 1);
        assert_eq!(bob_msg.attachments[0].name, "screenshot.png");

        // DM message is received. DM folder is Groups/DM_dm456 → chat = "DM_dm456".
        let dm_msg = day10.iter().find(|m| m.guid == "dm001").unwrap();
        assert!(!dm_msg.from_me);
        assert_eq!(dm_msg.chat, "DM_dm456");

        // Hangouts rows carry the legacy label in labels[], NOT in service.
        let h_msg = day10.iter().find(|m| m.guid == "conv_abc/ev001").unwrap();
        assert!(h_msg.labels.contains(&"hangouts-legacy".to_string()),
            "labels should contain hangouts-legacy: {:?}", h_msg.labels);
        assert_eq!(h_msg.service, "", "service should be empty, not hangouts-legacy");
        assert_eq!(h_msg.text, "Legacy message!");
        assert_eq!(h_msg.chat, "conv_abc");
        assert_eq!(h_msg.chat_name, "Old Friends");
        assert_eq!(h_msg.sender_name, "Alice Old");

        let _ = fs::remove_file(zip_path);
    }

    #[test]
    fn reimport_is_noop() {
        let v = temp_vault("reimport");
        let zip_path = fake_gchat_zip("reimport");

        run(&v, &zip_path, Some("alice@example.com"));

        let month_file = v.root().join("correspondence/google-chat/2021-05.jsonl");
        let content_before = fs::read_to_string(&month_file).unwrap();

        let out2 = run(&v, &zip_path, Some("alice@example.com"));
        assert_eq!(out2.counts.get("messages"), Some(&0), "no new messages on re-import");
        assert_eq!(out2.counts.get("hangouts"), Some(&0));
        assert_eq!(out2.counts.get("duplicates"), Some(&5), "all 5 rows are duplicates");

        let content_after = fs::read_to_string(&month_file).unwrap();
        assert_eq!(content_before, content_after, "vault unchanged on re-import");

        let _ = fs::remove_file(zip_path);
    }

    #[test]
    fn old_lines_still_deserialize() {
        // Back-compat: a pre-existing correspondence line (no Chat-specific
        // fields) must still parse as Message.
        let line = r#"{"ts":"2021-05-10T10:23:00-07:00","source":"google-chat","chat":"space123","from_me":true,"kind":"message","text":"hello"}"#;
        let m: crate::correspondence::Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hello");
        assert!(m.from_me);
    }
}
