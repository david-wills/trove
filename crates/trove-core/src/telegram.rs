//! Telegram — Import of the official Telegram Desktop JSON export
//! (`result.json`).
//!
//! The user generates the export via Telegram Desktop → Settings → Advanced
//! → Export Telegram Data → JSON format. The archive unpacks to a folder
//! containing `result.json` (or `results.json`) plus any exported media files.
//! Trove only parses the JSON; media files stay on disk as metadata references.
//!
//! **Export shape (official schema — core.telegram.org/import-export):**
//!
//! ```text
//! result.json
//! └── chats.list[]
//!     ├── id             integer  — Telegram chat/channel id
//!     ├── name           string   — display name
//!     ├── type           string   — personal_chat / private_group / etc.
//!     └── messages[]
//!         ├── id              integer  — message id (unique within chat)
//!         ├── type            string   — "message" | "service"
//!         ├── date            string   — ISO 8601, NO timezone (exporting machine's wall-clock)
//!         ├── date_unixtime   string   — UTC epoch seconds (authoritative; prefer over `date`)
//!         ├── from            string   — sender display name (absent on service msgs)
//!         ├── from_id         string   — "user<id>" | "channel<id>"
//!         ├── text            string | array<TextPart>
//!         ├── text_entities   array<{type,text,...}>
//!         ├── forwarded_from  string (optional)
//!         ├── reply_to_message_id  integer (optional)
//!         ├── media_type      string (optional) — sticker/video_message/…
//!         ├── file            string (optional) — relative path
//!         ├── photo           string (optional) — relative path
//!         ├── sticker_emoji   string (optional)
//!         ├── action          string (optional, service msgs)
//!         └── actor           string (optional, service msgs)
//! ```
//!
//! **Timestamp note:** `date` is the exporting machine's local wall-clock time
//! with NO timezone marker — it is NOT UTC and differs by the user's offset.
//! `date_unixtime` is a string UTC epoch (e.g. "1693753543") — the only
//! unambiguous instant. The importer always prefers `date_unixtime` when
//! present; it falls back to `date` only for older exports that lack it
//! (ambiguity is documented in caveats).
//!
//! **Text** is a `string` when the message has no entities, or an array of
//! `{"type":"...","text":"..."}` objects / bare strings when entities are
//! present. Both shapes are normalised to a plain UTF-8 string (entity ranges
//! are decorative metadata that Trove does not need to store separately).
//!
//! **guid** is `"<chat_id>-<msg_id>"` — stable across re-imports of the same
//! or overlapping exports. Re-imports skip guids already stored (pure no-op).
//!
//! The import is **privacy-sensitive** (message bodies): it is Import-gated
//! (user-initiated), never automatic.
//!
//! MTProto / Takeout incremental pull is a **separate later spike** — the
//! grammers crate maturity and BYO `api_id` (ToS) must be confirmed first.
//!
//! Messages land in `correspondence/telegram/YYYY-MM.jsonl`.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::Value;

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/telegram"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "telegram",
        name: "Telegram",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your full Telegram message history from the official Desktop export \
                      (result.json). Re-runnable: duplicate message ids are skipped on each import.",
        domain: "correspondence",
        vault_path: "correspondence/telegram/",
        toggleable: false,
        setup: &[
            "Telegram Desktop → Settings → Advanced → Export Telegram Data.",
            "Choose JSON format and select the chats you want. The export produces a folder \
             containing result.json plus any media files.",
            "Drop the result.json file (or the whole folder) here. Media files are never \
             downloaded — only their filenames and sizes are stored as metadata.",
        ],
        caveats: "The export is a manual one-time snapshot — re-export and re-import to pick up \
                  new messages (duplicates are skipped automatically). A live incremental pull via \
                  the MTProto Takeout API is a planned later addition; it requires a personal \
                  api_id registered at my.telegram.org (Telegram's terms of service prohibit \
                  compiled-in application credentials). \
                  Timestamps use the authoritative UTC epoch (date_unixtime) when present; \
                  very old exports that carry only the naive 'date' field are stored with the \
                  importer's current local offset (minor ambiguity across DST / timezone changes). \
                  Without supplying your Telegram display name or id in the optional 'me' field, \
                  your own sent messages will not be marked as sent (from_me will be false for all messages).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["json"],
    params: &[crate::registry::ImportParam {
        key: "me",
        label: "Your Telegram display name or id",
        placeholder: "display name or numeric id (optional, marks your own messages)",
        required: false,
    }],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Deserialization shapes for result.json.

/// Top-level result.json wrapper — only the chats field is required; every
/// other top-level key (personal_information, contacts, sessions, …) is
/// ignored and left to a future import phase if needed.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct TelegramExport {
    #[serde(default)]
    chats: TelegramChats,
    /// Some exports nest left chats under `left_chats` with the same shape.
    #[serde(default)]
    left_chats: Option<TelegramChats>,
}

#[derive(Debug, Default, Deserialize)]
struct TelegramChats {
    #[serde(default)]
    list: Vec<TelegramChat>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct TelegramChat {
    /// Telegram's numeric chat/channel id (often serialised as a JSON integer).
    #[serde(default)]
    id: Value, // i64 or string in the wild; normalised to string for guid
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    chat_type: String,
    #[serde(default)]
    messages: Vec<TelegramMessage>,
}

/// One message row — all optional fields default to empty/zero so a schema
/// extension in a newer export version never causes a parse failure.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct TelegramMessage {
    #[serde(default)]
    id: Value, // integer in JSON; convert to string for guid
    #[serde(rename = "type", default)]
    msg_type: String,
    /// ISO 8601 with NO timezone marker — the exporting machine's wall-clock.
    /// Ambiguous across timezones; prefer `date_unixtime` when present.
    #[serde(default)]
    date: String,
    /// UTC epoch seconds as a decimal string (e.g. "1693753543").
    /// Present in all modern Telegram Desktop exports; authoritative over `date`.
    #[serde(default)]
    date_unixtime: String,
    /// sender display name (absent on service messages)
    #[serde(default)]
    from: String,
    /// "user12345" or "channel12345"
    #[serde(default)]
    from_id: String,
    /// Either a plain `"string"` or an `[{"type":…,"text":…}, …]` array.
    #[serde(default)]
    text: Value,
    /// Redundant with `text`; carried for completeness, not separately stored.
    #[serde(default)]
    text_entities: Value,
    #[serde(default)]
    forwarded_from: String,
    #[serde(default)]
    reply_to_message_id: Value, // integer or absent
    /// sticker / video_message / voice_message / animation / video_file / audio_file / …
    #[serde(default)]
    media_type: String,
    /// Relative path to the exported file, if any.
    #[serde(default)]
    file: String,
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    file_size: Value, // integer bytes
    /// Relative path to the exported photo.
    #[serde(default)]
    photo: String,
    /// Emoji for a sticker message.
    #[serde(default)]
    sticker_emoji: String,
    /// MIME type of the file/photo, if the export includes it.
    #[serde(default)]
    mime_type: String,
    /// Service message action (create_group, pin_message, …).
    #[serde(default)]
    action: String,
    /// Actor for service messages (display name).
    #[serde(default)]
    actor: String,
}

// ---------------------------------------------------------------------------
// Timestamp parsing.

/// Telegram Desktop exports ISO 8601 strings without a timezone marker
/// (e.g. "2026-06-10T14:03:01") — these are local times on the exporting
/// machine. We parse them as-is and re-format as local RFC3339 with the
/// current timezone offset (the same convention as every other import).
fn tg_ts_to_local(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Try RFC3339 with an explicit offset first (some clients include one).
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Local).to_rfc3339());
    }
    // Try "YYYY-MM-DDTHH:MM:SS" (no offset) — treat as local time.
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        use chrono::TimeZone;
        return Local.from_local_datetime(&t).earliest().map(|dt| dt.to_rfc3339());
    }
    // Fallback: "YYYY-MM-DD HH:MM:SS" (space separator).
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        use chrono::TimeZone;
        return Local.from_local_datetime(&t).earliest().map(|dt| dt.to_rfc3339());
    }
    None
}

// ---------------------------------------------------------------------------
// Text normalisation.

/// Flatten a Telegram `text` value (string or entity array) to a plain string.
///
/// A plain-text message: `"hello world"` → `"hello world"`.
/// An entity-annotated message: `[{"type":"bold","text":"hi"}, " there"]` →
/// `"hi there"` — entities are decorative, the vault stores plain text.
fn flatten_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                match part {
                    Value::String(s) => out.push_str(s),
                    Value::Object(obj) => {
                        if let Some(Value::String(t)) = obj.get("text") {
                            out.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// Extract the numeric id from a Telegram id value (integer or string).
fn id_str(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Importer.

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
        .filter(|m| !m.is_empty());

    // Accept both a direct result.json and a folder containing result.json /
    // results.json (the export folder name varies).
    let json_path: std::path::PathBuf = if path.is_dir() {
        // Try "result.json" then "results.json" inside the folder.
        let r1 = path.join("result.json");
        let r2 = path.join("results.json");
        if r1.exists() {
            r1
        } else if r2.exists() {
            r2
        } else {
            anyhow::bail!(
                "No result.json or results.json found inside {}",
                path.display()
            );
        }
    } else {
        path.to_path_buf()
    };

    let body = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;

    let export: TelegramExport = serde_json::from_str(&body)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    // Collect all chats (active + left).
    let mut all_chats: Vec<TelegramChat> = export.chats.list;
    if let Some(left) = export.left_chats {
        all_chats.extend(left.list);
    }

    let mut seen = vault.correspondence_guids("telegram")?;
    let mut messages: Vec<Message> = Vec::new();
    let mut total_messages: u64 = 0;
    let mut total_service: u64 = 0;
    let mut total_duplicates: u64 = 0;

    for (chat_idx, chat) in all_chats.iter().enumerate() {
        let chat_id = id_str(&chat.id);
        let chat_name = chat.name.clone();
        // When chat_id is empty (malformed export) use a stable per-chat key
        // derived from the chat's name + its position to prevent cross-chat
        // guid collisions from shared message ids.
        let guid_prefix = if chat_id.is_empty() {
            format!("anon{}", chat_idx)
        } else {
            chat_id.clone()
        };

        for msg in &chat.messages {
            let msg_id = id_str(&msg.id);
            if msg_id.is_empty() {
                continue;
            }
            // guid = "<chat_id>-<msg_id>" — stable across re-imports.
            let guid = format!("{}-{}", guid_prefix, msg_id);

            // Resolve timestamp: prefer date_unixtime (UTC epoch, unambiguous)
            // over the naive date string (wall-clock, no tz offset).
            let ts_opt: Option<String> = if !msg.date_unixtime.is_empty() {
                msg.date_unixtime.trim().parse::<i64>().ok().and_then(|secs| {
                    DateTime::from_timestamp(secs, 0)
                        .map(|t| t.with_timezone(&Local).to_rfc3339())
                })
            } else {
                // Fallback: older export without date_unixtime.
                tg_ts_to_local(&msg.date)
            };
            let Some(ts) = ts_opt else {
                continue;
            };

            if !seen.insert(guid.clone()) {
                total_duplicates += 1;
                continue;
            }

            // Determine from_me based on the optional `me` hint.
            // The from_id is "user<numeric_id>" — strip the prefix for comparison.
            let from_numeric = msg.from_id.trim_start_matches("user");
            let from_me = me.is_some_and(|m| {
                m.eq_ignore_ascii_case(&msg.from)
                    || m == from_numeric
                    || m == msg.from_id.as_str()
            });

            let mut m = Message::new("telegram", ts.clone());
            m.guid = guid;
            // chat = "<type>/<id>" so the chat key is unambiguous across types.
            m.chat = if chat_id.is_empty() {
                chat_name.clone()
            } else {
                chat_id.clone()
            };
            m.chat_name = chat_name.clone();
            m.from_me = from_me;

            if msg.msg_type == "service" {
                // Service message: group rename, pin, member change, etc.
                m.kind = "event".into();
                // Use action as text so the record is human-readable.
                let actor = if !msg.actor.is_empty() { msg.actor.clone() } else { msg.from.clone() };
                m.text = if !actor.is_empty() {
                    format!("{}: {}", actor, msg.action)
                } else {
                    msg.action.clone()
                };
                if m.text.trim().is_empty() {
                    // No meaningful content; skip.
                    seen.remove(&m.guid);
                    continue;
                }
                messages.push(m);
                total_service += 1;
                continue;
            }

            // Regular message.
            // sender_name = the sender's display name (msg.from), consistent
            // with discord.rs / facebook_messenger.rs / fastmail.rs convention.
            if !msg.from.is_empty() {
                m.sender_name = msg.from.clone();
            }
            if !from_me {
                m.sender = msg.from.clone();
            }

            m.text = flatten_text(&msg.text);

            // Attachment metadata (file or photo — never downloaded).
            let mut attachments: Vec<AttachmentMeta> = Vec::new();
            if !msg.file.is_empty() || !msg.photo.is_empty() {
                let path_field = if !msg.file.is_empty() { &msg.file } else { &msg.photo };
                let name = if !msg.file_name.is_empty() {
                    msg.file_name.clone()
                } else {
                    // Derive name from the relative path.
                    Path::new(path_field)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attachment".into())
                };
                let bytes: i64 = match &msg.file_size {
                    Value::Number(n) => n.as_i64().unwrap_or(0),
                    _ => 0,
                };
                attachments.push(AttachmentMeta {
                    name,
                    mime: msg.mime_type.clone(),
                    bytes,
                });
            }
            // Sticker: treat emoji as the text if no other text.
            if !msg.sticker_emoji.is_empty() && m.text.is_empty() {
                m.text = msg.sticker_emoji.clone();
            }
            m.attachments = attachments;

            // forwarded_from: the correspondence contract has no forwarded-from
            // field. We do NOT write forwarded_from into sender_name (that slot
            // must hold the real sender's display name). Forward provenance is
            // not stored separately — the message content is the original text.

            // reply_to_message_id → reply_to (as "<guid_prefix>-<reply_id>").
            let reply_id = id_str(&msg.reply_to_message_id);
            if !reply_id.is_empty() && reply_id != "0" {
                m.reply_to = format!("{}-{}", guid_prefix, reply_id);
            }

            if m.text.is_empty() && m.attachments.is_empty() {
                // No content (e.g. contact card or poll body — not yet parsed).
                seen.remove(&m.guid);
                continue;
            }

            messages.push(m);
            total_messages += 1;
        }
    }

    vault.append_messages(&messages)?;
    progress(ImportProgress {
        records: total_messages + total_service,
        percent: 100.0,
    });

    Ok(ImportOutcome {
        headline: format!(
            "{} messages, {} service events imported from {} chats, {} duplicates skipped",
            total_messages,
            total_service,
            all_chats.len(),
            total_duplicates,
        ),
        counts: [
            ("messages", total_messages),
            ("service_events", total_service),
            ("chats", all_chats.len() as u64),
            ("duplicates", total_duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-tg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path, me: Option<&str>) -> ImportOutcome {
        let mut params = BTreeMap::new();
        if let Some(m) = me {
            params.insert("me".to_string(), m.to_string());
        }
        (IMPORT.run)(v, path, &params, &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures.

    /// Minimal result.json with one personal chat and one group.
    fn fixture_result_json(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-tg-fix-{}-{name}.json", std::process::id()));
        fs::write(
            &path,
            r#"{
  "about": "Telegram export",
  "chats": {
    "about": "List of chats",
    "list": [
      {
        "id": 123456789,
        "name": "Alice",
        "type": "personal_chat",
        "messages": [
          {
            "id": 1,
            "type": "message",
            "date": "2026-06-10T09:00:00",
            "from": "David",
            "from_id": "user999",
            "text": "Hello Alice!"
          },
          {
            "id": 2,
            "type": "message",
            "date": "2026-06-10T09:01:00",
            "from": "Alice",
            "from_id": "user123456789",
            "text": "Hey David!",
            "reply_to_message_id": 1
          },
          {
            "id": 3,
            "type": "message",
            "date": "2026-06-10T09:02:00",
            "from": "Alice",
            "from_id": "user123456789",
            "text": [
              {"type": "bold", "text": "Bold text"},
              " and plain text"
            ]
          }
        ]
      },
      {
        "id": 987654321,
        "name": "My Group",
        "type": "private_group",
        "messages": [
          {
            "id": 10,
            "type": "message",
            "date": "2026-06-11T10:00:00",
            "from": "David",
            "from_id": "user999",
            "text": "Hi group!"
          },
          {
            "id": 11,
            "type": "service",
            "date": "2026-06-11T10:05:00",
            "actor": "David",
            "actor_id": "user999",
            "action": "create_group",
            "text": ""
          }
        ]
      }
    ]
  }
}"#
            .as_bytes(),
        )
        .unwrap();
        path
    }

    /// result.json with forwarded message, media attachment, and sticker.
    fn fixture_media_json(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-tg-med-{}-{name}.json", std::process::id()));
        fs::write(
            &path,
            r#"{
  "chats": {
    "list": [
      {
        "id": 555,
        "name": "Bob",
        "type": "personal_chat",
        "messages": [
          {
            "id": 100,
            "type": "message",
            "date": "2026-06-12T08:00:00",
            "from": "Bob",
            "from_id": "user555",
            "text": "",
            "forwarded_from": "Some Channel",
            "media_type": "audio_file",
            "file": "chats/chat_02/audio/audio_1.ogg",
            "file_name": "audio_1.ogg",
            "file_size": 48000,
            "mime_type": "audio/ogg"
          },
          {
            "id": 101,
            "type": "message",
            "date": "2026-06-12T08:05:00",
            "from": "David",
            "from_id": "user999",
            "text": "",
            "media_type": "sticker",
            "sticker_emoji": "😀",
            "file": "chats/chat_02/stickers/sticker_1.webp"
          },
          {
            "id": 102,
            "type": "message",
            "date": "2026-06-12T08:10:00",
            "from": "Bob",
            "from_id": "user555",
            "photo": "chats/chat_02/photos/photo_1.jpg",
            "text": "Check this out"
          }
        ]
      }
    ]
  }
}"#
            .as_bytes(),
        )
        .unwrap();
        path
    }

    // -----------------------------------------------------------------------
    // Tests.

    #[test]
    fn tg_ts_parses_all_shapes() {
        // No-offset (most common Telegram export form).
        let t = tg_ts_to_local("2026-06-10T14:03:01").unwrap();
        assert!(t.starts_with("2026-06-10"), "local RFC3339: {t}");
        // With explicit offset.
        let t2 = tg_ts_to_local("2026-06-10T14:03:01+00:00").unwrap();
        assert!(t2.contains("2026-06-10"), "offset variant: {t2}");
        // Space separator.
        let t3 = tg_ts_to_local("2026-06-10 09:00:00").unwrap();
        assert!(t3.starts_with("2026-06-10"), "space variant: {t3}");
        assert!(tg_ts_to_local("").is_none());
        assert!(tg_ts_to_local("not-a-date").is_none());
    }

    #[test]
    fn flatten_text_handles_string_and_array() {
        // Plain string.
        let v = serde_json::json!("Hello world");
        assert_eq!(flatten_text(&v), "Hello world");
        // Entity array with mixed bare strings and objects.
        let v = serde_json::json!([
            {"type": "bold", "text": "Bold"},
            " and ",
            {"type": "italic", "text": "italic"}
        ]);
        assert_eq!(flatten_text(&v), "Bold and italic");
        // Null / empty array.
        assert_eq!(flatten_text(&Value::Null), "");
        assert_eq!(flatten_text(&serde_json::json!([])), "");
    }

    #[test]
    fn basic_import_parses_two_chats() {
        let v = temp_vault("basic");
        let fix = fixture_result_json("basic");
        let out = run(&v, &fix, Some("David"));

        assert_eq!(out.counts.get("chats"), Some(&2));
        assert_eq!(out.counts.get("messages"), Some(&4)); // 3 in personal + 1 in group
        assert_eq!(out.counts.get("service_events"), Some(&1)); // create_group
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Personal chat messages are in the June file.
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3, "three messages on 2026-06-10: {day:?}");

        let first = day.iter().find(|m| m.guid == "123456789-1").unwrap();
        assert!(first.from_me, "David is 'me'");
        assert_eq!(first.text, "Hello Alice!");
        assert_eq!(first.chat, "123456789");
        assert_eq!(first.chat_name, "Alice");
        assert_eq!(first.kind, "message");

        let reply = day.iter().find(|m| m.guid == "123456789-2").unwrap();
        assert!(!reply.from_me);
        assert_eq!(reply.sender, "Alice");
        assert_eq!(reply.reply_to, "123456789-1", "reply_to points to first msg");

        // Entity-annotated text flattened correctly.
        let entity_msg = day.iter().find(|m| m.guid == "123456789-3").unwrap();
        assert_eq!(entity_msg.text, "Bold text and plain text");

        let _ = fs::remove_file(fix);
    }

    #[test]
    fn service_event_stored_as_event_kind() {
        let v = temp_vault("service");
        let fix = fixture_result_json("service");
        run(&v, &fix, Some("David"));

        let day = v.correspondence_timeline("2026-06-11").unwrap();
        let events: Vec<_> = day.iter().filter(|m| m.kind == "event").collect();
        assert_eq!(events.len(), 1);
        assert!(events[0].text.contains("create_group"), "action in text: {}", events[0].text);
        assert!(events[0].text.contains("David"), "actor in text: {}", events[0].text);

        let _ = fs::remove_file(fix);
    }

    #[test]
    fn media_forwarded_and_sticker() {
        let v = temp_vault("media");
        let fix = fixture_media_json("media");
        let out = run(&v, &fix, Some("David"));

        assert_eq!(out.counts.get("messages"), Some(&3)); // all 3 have content
        assert_eq!(out.counts.get("chats"), Some(&1));

        let day = v.correspondence_timeline("2026-06-12").unwrap();
        assert_eq!(day.len(), 3);

        // Audio file forwarded from a channel — sender_name is the real sender,
        // not the forwarded_from provenance (which has no contract field).
        let audio = day.iter().find(|m| m.guid == "555-100").unwrap();
        assert!(!audio.from_me);
        assert_eq!(audio.attachments.len(), 1);
        assert_eq!(audio.attachments[0].name, "audio_1.ogg");
        assert_eq!(audio.attachments[0].bytes, 48000);
        assert_eq!(audio.attachments[0].mime, "audio/ogg");
        assert_eq!(audio.sender_name, "Bob", "sender_name = real sender, not fwd provenance");

        // Sticker: emoji as text, file as attachment.
        let sticker = day.iter().find(|m| m.guid == "555-101").unwrap();
        assert!(sticker.from_me, "David's sticker");
        assert_eq!(sticker.text, "😀", "sticker emoji as text");
        assert_eq!(sticker.attachments[0].name, "sticker_1.webp");

        // Photo with caption.
        let photo = day.iter().find(|m| m.guid == "555-102").unwrap();
        assert_eq!(photo.text, "Check this out");
        assert_eq!(photo.attachments[0].name, "photo_1.jpg");

        let _ = fs::remove_file(fix);
    }

    #[test]
    fn reimport_is_noop() {
        let v = temp_vault("reimport");
        let fix = fixture_result_json("reimport");

        let first = run(&v, &fix, Some("David"));
        let total_first = first.counts.get("messages").copied().unwrap_or(0)
            + first.counts.get("service_events").copied().unwrap_or(0);

        let second = run(&v, &fix, Some("David"));
        assert_eq!(second.counts.get("messages"), Some(&0));
        assert_eq!(second.counts.get("service_events"), Some(&0));
        assert_eq!(second.counts.get("duplicates"), Some(&total_first));

        let _ = fs::remove_file(fix);
    }

    #[test]
    fn left_chats_also_imported() {
        let v = temp_vault("leftchats");
        let path = std::env::temp_dir()
            .join(format!("trove-tg-leftchats-{}.json", std::process::id()));
        fs::write(
            &path,
            r#"{
  "chats": { "list": [] },
  "left_chats": {
    "list": [
      {
        "id": 77,
        "name": "Old Group",
        "type": "private_group",
        "messages": [
          {
            "id": 5,
            "type": "message",
            "date": "2025-01-15T10:00:00",
            "from": "David",
            "from_id": "user999",
            "text": "bye"
          }
        ]
      }
    ]
  }
}"#
            .as_bytes(),
        )
        .unwrap();
        let out = run(&v, &path, Some("David"));
        assert_eq!(out.counts.get("messages"), Some(&1));
        let msgs = v.correspondence_timeline("2025-01-15").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].guid, "77-5");
        assert_eq!(msgs[0].chat_name, "Old Group");
        assert!(msgs[0].from_me);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn old_message_lines_still_deserialize() {
        // Back-compat: a correspondence line without telegram-specific fields
        // still deserializes as a Message (serde default fields).
        let line = r#"{"ts":"2026-06-10T09:00:00-07:00","source":"telegram","chat":"123456789","from_me":true,"kind":"message","text":"Hello Alice!"}"#;
        let m: crate::correspondence::Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "Hello Alice!");
        assert!(m.from_me);
    }

    /// date_unixtime (UTC epoch) takes precedence over the naive `date` string.
    /// The official example from core.telegram.org/import-export:
    ///   date "2023-09-03T17:05:43" (UTC+2 wall-clock) vs
    ///   date_unixtime "1693753543" (= 2023-09-03T15:05:43Z = 17:05:43 CEST).
    /// Importing on any machine must produce a timestamp equivalent to 15:05:43 UTC.
    #[test]
    fn date_unixtime_preferred_over_naive_date() {
        let v = temp_vault("ts_pref");
        let path = std::env::temp_dir()
            .join(format!("trove-tg-ts-{}.json", std::process::id()));
        // Epoch 1693753543 = 2023-09-03T15:05:43Z.
        // The naive `date` field shows a different (wall-clock) time; it must
        // NOT be used when date_unixtime is present.
        fs::write(
            &path,
            r#"{
  "chats": {
    "list": [
      {
        "id": 42,
        "name": "Test",
        "type": "personal_chat",
        "messages": [
          {
            "id": 1,
            "type": "message",
            "date": "2023-09-03T17:05:43",
            "date_unixtime": "1693753543",
            "from": "Alice",
            "from_id": "user42",
            "text": "epoch test"
          }
        ]
      }
    ]
  }
}"#
            .as_bytes(),
        )
        .unwrap();
        let out = run(&v, &path, None);
        assert_eq!(out.counts.get("messages"), Some(&1));
        let msgs = v.correspondence_timeline("2023-09-03").unwrap();
        assert_eq!(msgs.len(), 1, "message stored on correct UTC date");
        // The stored timestamp must encode the correct UTC instant regardless of
        // the importing machine's timezone.
        let ts = &msgs[0].ts;
        // Parse the stored RFC3339 and confirm UTC equivalent is 15:05:43Z.
        let parsed = DateTime::parse_from_rfc3339(ts)
            .expect("stored ts is RFC3339");
        use chrono::Timelike;
        let utc = parsed.with_timezone(&chrono::Utc);
        assert_eq!(utc.hour(), 15, "UTC hour must be 15 (epoch 1693753543)");
        assert_eq!(utc.minute(), 5);
        assert_eq!(utc.second(), 43);
        let _ = fs::remove_file(path);
    }

    /// sender_name always holds the real sender display name, not forwarded_from.
    #[test]
    fn sender_name_is_real_sender_not_forwarded_from() {
        let v = temp_vault("sndname");
        let path = std::env::temp_dir()
            .join(format!("trove-tg-snd-{}.json", std::process::id()));
        fs::write(
            &path,
            r#"{
  "chats": {
    "list": [
      {
        "id": 99,
        "name": "Carol",
        "type": "personal_chat",
        "messages": [
          {
            "id": 1,
            "type": "message",
            "date": "2026-01-01T12:00:00",
            "from": "Carol",
            "from_id": "user99",
            "forwarded_from": "Some News Channel",
            "text": "forwarded article"
          }
        ]
      }
    ]
  }
}"#
            .as_bytes(),
        )
        .unwrap();
        run(&v, &path, None);
        let msgs = v.correspondence_timeline("2026-01-01").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender_name, "Carol", "sender_name = real sender, not forwarded_from");
        assert!(!msgs[0].sender_name.contains("Some News Channel"), "fwd provenance must not appear in sender_name");
        let _ = fs::remove_file(path);
    }
}
