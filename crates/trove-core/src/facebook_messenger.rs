//! Facebook Messenger — a file-import source ([`Behavior::Import`]).
//!
//! The only sanctioned path is the Meta **Download Your Information** (DYI)
//! JSON export: Settings → Your Facebook Information → Download Your
//! Information → Messages, format **JSON**. The export is standalone-clean:
//! no auth, no network, no TCC.
//!
//! ## ZIP layout
//!
//! ```text
//! messages/
//!   inbox/<ConversationName>_<hash>/
//!     message_1.json          ← first shard (newest messages first)
//!     message_2.json          ← older shard (present if the thread is large)
//!   archived_threads/…        ← same structure, archived conversations
//!   filtered_threads/…        ← message requests / spam
//!   e2ee_cutover/…            ← end-to-end encrypted thread migration
//! ```
//!
//! Each `message_N.json` contains:
//! ```json
//! {
//!   "participants": [{"name": "Alice"}, {"name": "David"}],
//!   "messages": [
//!     {"sender_name": "Alice",
//!      "timestamp_ms": 1700000000000,
//!      "content": "Hey!",
//!      "reactions": [{"reaction": "😍", "actor": "David"}],
//!      "photos": [{"uri": "messages/inbox/…/photos/123.jpg"}],
//!      "share": {"link": "https://…", "share_text": "…"}}
//!   ],
//!   "title": "Conversation With Alice"
//! }
//! ```
//!
//! Shards for the same conversation are merged in order (the export splits
//! large threads into numbered files; all shards carry the same `title` +
//! `participants`). The full export may also include a full-account messenger
//! ZIP that only contains `messages/` paths from a combined Facebook export —
//! both shapes are handled.
//!
//! ## What lands where
//!
//! All messages and reactions land in the unified `correspondence` stream
//! (`correspondence/facebook-messenger/YYYY-MM.jsonl`) using the
//! [`crate::correspondence::Message`] contract.
//!
//! ## Dedupe key
//!
//! The Meta export carries **no stable message id** — the research note and
//! the brief both document this. The `guid` is a `sha256` of
//! `(chat_folder, timestamp_ms, sender_name, content)`, length-prefixed to
//! prevent component-boundary collisions. Including content eliminates
//! same-sender/same-millisecond collisions (rapid-fire messages). The full
//! conversation folder name (with Meta's `_<hash>` suffix) is the chat key so
//! identically-named contacts are never merged.
//!
//! ## Owner identification
//!
//! From Meta's export docs and community research: the export owner's name
//! appears in the `participants` list, but the export is structured so that
//! the owner's messages are identifiable because **only their own name appears
//! in `sender_name`**. The import therefore requires an explicit owner name
//! (`me` param) so `from_me` is set correctly — the only reliable mechanism
//! without a stable id.
//!
//! ## Meta mojibake
//!
//! DYI strings are UTF-8 bytes mis-stored as Latin-1. Repaired via
//! [`crate::meta_encoding::fix_value`], which fixes every string value in
//! the parsed JSON recursively (see module docs for the exact rule). This is
//! the same decoder used by `facebook.rs` — built once, fixed once.
//!
//! ## Privacy
//!
//! Message bodies are private. An explicit opt-in acknowledgement is required
//! at import time (the `acknowledge` param). This matches the facebook.rs
//! pattern for the same DYI export.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};
use serde_json::Value;

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::meta_encoding::fix_value;
use crate::registry::{Behavior, ImportOutcome, ImportParam, ImportSpec, IntegrationDef};
use crate::vault::Vault;

const SOURCE: &str = "facebook-messenger";
const DIR: &str = "correspondence/facebook-messenger";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "facebook-messenger",
        name: "Facebook Messenger",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your Messenger conversations from Meta's Download Your \
                      Information export (facebook.com/dyi), including group threads, \
                      reactions, and attachment metadata. Re-runnable; re-importing \
                      never duplicates. Compatible with the Messages-only download or \
                      the combined Facebook DYI export.",
        domain: "correspondence",
        vault_path: "correspondence/facebook-messenger/",
        toggleable: false,
        setup: &[
            "Meta Accounts Center → Settings & Privacy → Your Facebook Information → \
             Download Your Information → select Messages (or the full export), format \
             JSON — not HTML. A ZIP is delivered by notification/email (typically \
             hours; up to 14 days for large accounts).",
            "Drop the ZIP here as-is. Message bodies are sensitive; import only if you \
             intend to store them in your private vault. Your display name (as Meta \
             knows it) is required so the importer can mark messages you sent.",
        ],
        caveats: "Meta encodes text as Latin-1-interpreted-as-UTF-8 (mojibake); the \
                  importer fixes this automatically. There is no stable message id in the \
                  export, so deduplication uses a hash of (conversation folder, timestamp, \
                  sender, content). The full folder name (including Meta's hash suffix) is \
                  used as the conversation key so identically-named contacts stay separate.",
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
            // 🔒 Opt-in acknowledgement — message bodies are sensitive.
            key: "acknowledge",
            label: "Privacy acknowledgement",
            placeholder: "Type 'yes' to confirm you want your Messenger history stored in your vault",
            required: true,
        },
        ImportParam {
            key: "me",
            label: "Your name as Meta knows it",
            placeholder: "e.g. David Wills — used to set sent/received correctly",
            required: true,
        },
    ],
    run: run_import,
};

// ---------------------------------------------------------------------------
// The importer.

#[derive(Default)]
struct Stats {
    messages: u64,
    reactions: u64,
    duplicates: u64,
    conversations: HashSet<String>,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let me = params
        .get("me")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // Load guids already stored so re-imports are pure no-ops.
    let mut seen = vault.correspondence_guids(SOURCE)?;

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a Meta DYI export ZIP?", path.display()))?;

    // Collect all shard paths that belong to Messenger conversations.
    // Group them by conversation folder so we can merge shards.
    let mut by_conv: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    for name in &names {
        // Match the four Messenger subtrees the brief documents:
        // messages/inbox/, messages/archived_threads/, messages/filtered_threads/,
        // messages/e2ee_cutover/ — all have the same per-conv/message_N.json layout.
        if let Some(rest) = messenger_subtree_rest(name) {
            if !rest.to_ascii_lowercase().ends_with(".json") {
                continue; // photos/ and other subdirs — skip.
            }
            // We want only message_N.json files, not channel/participant metadata.
            let filename = rest.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
            if !filename.starts_with("message_") {
                continue;
            }
            // The conversation folder is everything before the last '/'.
            let conv_folder = rest
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or(rest)
                .to_string();
            by_conv.entry(conv_folder).or_default().push(name.clone());
        }
    }

    // Sort shards within each conversation (message_1 < message_2 < …).
    for shards in by_conv.values_mut() {
        shards.sort();
    }

    let mut all_messages: Vec<Message> = Vec::new();
    let mut stats = Stats::default();

    for (conv_key, shards) in &by_conv {
        // Use the FULL conversation folder name as the chat key — the `_<hash>`
        // suffix is Meta's disambiguator for identically-named contacts (e.g.
        // `johnsmith_aaaa1111` vs `johnsmith_bbbb2222`). Stripping it merges
        // unrelated threads. Matches instagram.rs which keeps the full folder name.
        let chat_id = conv_key.clone();
        let mut chat_name = String::new();

        for shard_path in shards {
            let mut body = String::new();
            if read_entry(&mut zip, shard_path, &mut body).is_err() {
                continue;
            }
            let Ok(mut value) = serde_json::from_str::<Value>(&body) else {
                continue;
            };
            // Repair Meta mojibake recursively across all string values.
            fix_value(&mut value);

            // Extract the display title from the first shard that has one.
            if chat_name.is_empty() {
                if let Some(t) = value.get("title").and_then(Value::as_str) {
                    if !t.is_empty() {
                        chat_name = t.to_string();
                    }
                }
            }

            let Some(messages_arr) = value.get("messages").and_then(Value::as_array) else {
                continue;
            };

            for msg in messages_arr {
                let Some(msg_obj) = msg.as_object() else { continue };

                let ts_ms = match msg_obj.get("timestamp_ms").and_then(Value::as_i64) {
                    Some(ms) => ms,
                    None => continue, // no timestamp — cannot partition.
                };
                let sender = msg_obj
                    .get("sender_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let content = msg_obj
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                // guid = sha256(chat_id | timestamp_ms | sender_name | content) — length-prefixed.
                // Including content prevents same-sender/same-ms collisions; mirrors instagram.rs.
                let guid = message_guid(&chat_id, ts_ms, &sender, &content);

                // Handle reactions: each reaction in the array becomes a
                // kind:"reaction" correspondence row.
                if let Some(rxs) = msg_obj.get("reactions").and_then(Value::as_array) {
                    for rx in rxs.iter() {
                        let rx_emoji = rx
                            .get("reaction")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let rx_actor = rx
                            .get("actor")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if rx_emoji.is_empty() {
                            continue;
                        }
                        // Key on (msg_guid, actor, emoji) — stable across re-exports even
                        // if sibling reactions are added/removed (index-based keys would
                        // shift). An actor can only place one emoji reaction per message.
                        let rx_guid = {
                            let mut h = Sha256::new();
                            for part in [guid.as_str(), rx_actor.as_str(), rx_emoji.as_str()] {
                                h.update((part.len() as u64).to_le_bytes());
                                h.update(part.as_bytes());
                            }
                            format!("{:x}", h.finalize())
                        };
                        if !seen.insert(rx_guid.clone()) {
                            stats.duplicates += 1;
                            continue;
                        }
                        let ts = ts_ms_to_local(ts_ms);
                        let mut m = Message::new(SOURCE, ts);
                        m.guid = rx_guid;
                        m.chat = chat_id.clone();
                        m.chat_name = chat_name.clone();
                        m.kind = "reaction".into();
                        m.reaction = rx_emoji;
                        m.reply_to = guid.clone();
                        // The reaction actor: if it's the owner it's from_me.
                        if !rx_actor.is_empty() {
                            m.sender_name = rx_actor.clone();
                            m.from_me = !me.is_empty() && me.eq_ignore_ascii_case(&rx_actor);
                            if !m.from_me {
                                m.sender = rx_actor;
                            }
                        }
                        m.service = "Facebook Messenger".into();
                        all_messages.push(m);
                        stats.reactions += 1;
                    }
                }

                // Main message row.
                let has_content = !content.is_empty();
                let has_photos = msg_obj.get("photos").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
                let has_share = msg_obj.get("share").is_some();
                let has_audio = msg_obj.get("audio_files").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
                let has_videos = msg_obj.get("videos").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
                let has_gifs = msg_obj.get("gifs").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
                let has_sticker = msg_obj.get("sticker").is_some();

                // Keep messages that have at least some content to write.
                if !has_content && !has_photos && !has_share && !has_audio && !has_videos && !has_gifs && !has_sticker {
                    continue;
                }

                if !seen.insert(guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }

                let ts = ts_ms_to_local(ts_ms);
                let from_me = !me.is_empty() && me.eq_ignore_ascii_case(&sender);

                let mut m = Message::new(SOURCE, ts);
                m.guid = guid.clone();
                m.chat = chat_id.clone();
                m.chat_name = chat_name.clone();
                m.sender_name = sender.clone();
                m.from_me = from_me;
                if !from_me {
                    m.sender = sender.clone();
                }
                m.text = content;
                m.service = "Facebook Messenger".into();

                // Attachments: photos, videos, audio_files, gifs — metadata only.
                let mut attachments: Vec<AttachmentMeta> = Vec::new();

                if let Some(photos) = msg_obj.get("photos").and_then(Value::as_array) {
                    for p in photos {
                        let uri = p.get("uri").and_then(Value::as_str).unwrap_or("");
                        if !uri.is_empty() {
                            let name = uri.rsplit('/').next().unwrap_or("photo").to_string();
                            attachments.push(AttachmentMeta { name, mime: "image".into(), bytes: 0 });
                        }
                    }
                }
                if let Some(videos) = msg_obj.get("videos").and_then(Value::as_array) {
                    for vid in videos {
                        let uri = vid.get("uri").and_then(Value::as_str).unwrap_or("");
                        if !uri.is_empty() {
                            let name = uri.rsplit('/').next().unwrap_or("video").to_string();
                            attachments.push(AttachmentMeta { name, mime: "video".into(), bytes: 0 });
                        }
                    }
                }
                if let Some(audios) = msg_obj.get("audio_files").and_then(Value::as_array) {
                    for a in audios {
                        let uri = a.get("uri").and_then(Value::as_str).unwrap_or("");
                        if !uri.is_empty() {
                            let name = uri.rsplit('/').next().unwrap_or("audio").to_string();
                            attachments.push(AttachmentMeta { name, mime: "audio".into(), bytes: 0 });
                        }
                    }
                }
                if let Some(gifs) = msg_obj.get("gifs").and_then(Value::as_array) {
                    for g in gifs {
                        let uri = g.get("uri").and_then(Value::as_str).unwrap_or("");
                        if !uri.is_empty() {
                            let name = uri.rsplit('/').next().unwrap_or("animation.gif").to_string();
                            attachments.push(AttachmentMeta { name, mime: "image/gif".into(), bytes: 0 });
                        }
                    }
                }
                // Share link: treat as an attachment with the link URI as the name.
                if let Some(share) = msg_obj.get("share").and_then(Value::as_object) {
                    let link = share.get("link").and_then(Value::as_str).unwrap_or("");
                    let share_text = share.get("share_text").and_then(Value::as_str).unwrap_or("");
                    // If there's no content yet, use share_text as the message text.
                    if m.text.is_empty() && !share_text.is_empty() {
                        m.text = share_text.to_string();
                    }
                    if !link.is_empty() {
                        attachments.push(AttachmentMeta {
                            name: link.to_string(),
                            mime: "link".into(),
                            bytes: 0,
                        });
                    }
                }
                if let Some(sticker) = msg_obj.get("sticker").and_then(Value::as_object) {
                    let uri = sticker.get("uri").and_then(Value::as_str).unwrap_or("");
                    if !uri.is_empty() {
                        let name = uri.rsplit('/').next().unwrap_or("sticker").to_string();
                        attachments.push(AttachmentMeta { name, mime: "sticker".into(), bytes: 0 });
                    }
                }

                m.attachments = attachments;
                all_messages.push(m);
                stats.messages += 1;
                stats.conversations.insert(chat_id.clone());
            }
        }
    }

    vault.append_messages(&all_messages)?;
    progress(ImportProgress { records: stats.messages + stats.reactions, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{} messages, {} reactions imported from {} conversations, {} duplicates skipped",
            stats.messages,
            stats.reactions,
            stats.conversations.len(),
            stats.duplicates,
        ),
        counts: [
            ("messages", stats.messages),
            ("reactions", stats.reactions),
            ("conversations", stats.conversations.len() as u64),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers.

/// Returns the path portion after any of the four Messenger subtree prefixes.
/// Returns `None` when the path isn't in a Messenger subtree.
fn messenger_subtree_rest(name: &str) -> Option<&str> {
    for prefix in &[
        "messages/inbox/",
        "messages/archived_threads/",
        "messages/filtered_threads/",
        "messages/e2ee_cutover/",
    ] {
        if let Some(rest) = strip_prefix_in_path(name, prefix) {
            return Some(rest);
        }
    }
    None
}

/// Find `prefix` anywhere in `path` (handling a possible root component like
/// `your_activity_across_facebook/`) and return the slice after it.
///
/// The four DYI prefixes are fixed ASCII, so case-sensitive search on the
/// original string is correct and avoids any byte-offset mismatch that would
/// arise from finding a position in a lowercased copy and slicing the original.
fn strip_prefix_in_path<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    path.find(prefix).map(|pos| &path[pos + prefix.len()..])
}

/// Epoch milliseconds → local RFC3339.
fn ts_ms_to_local(ts_ms: i64) -> String {
    DateTime::from_timestamp_millis(ts_ms)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|| format!("{ts_ms}"))
}

/// A length-prefixed sha256 over (chat_id, timestamp_ms, sender_name, content) as hex.
/// Including content prevents same-sender/same-ms collisions within one conversation
/// (rapid-fire messages share a millisecond). Mirrors instagram.rs msg_guid.
fn message_guid(chat_id: &str, ts_ms: i64, sender: &str, content: &str) -> String {
    let mut h = Sha256::new();
    for part in [chat_id, &ts_ms.to_string(), sender, content] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}

/// Read one zip entry into `body` as a UTF-8 string.
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
        let dir =
            std::env::temp_dir().join(format!("trove-fbm-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run_params(me: &str) -> BTreeMap<String, String> {
        [
            ("acknowledge".to_string(), "yes".to_string()),
            ("me".to_string(), me.to_string()),
        ]
        .into()
    }

    fn run(v: &Vault, path: &Path, me: &str) -> ImportOutcome {
        (IMPORT.run)(v, path, &run_params(me), &mut |_| {}).unwrap()
    }

    // ------------------------------------------------------------------
    // Fixture builders.

    /// Build a synthetic Meta DYI Messenger ZIP.
    ///
    /// Conversations:
    ///   1. `alice_abc12345/message_1.json` + `message_2.json` — two-shard
    ///      split. Shard 1: a regular message, one with a reaction, one
    ///      with a photo. Shard 2: one older message.
    ///   2. `groupchat_def67890/message_1.json` — a group chat with a share link.
    fn build_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-fbm-zip-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Conversation 1 — shard 1 (newer messages).
        // "ð" (U+00F0 U+009F U+0098 U+008D) is mojibake for 😍 (U+1F60D).
        // "cafÃ©" (U+0063 U+0061 U+0066 U+00C3 U+00A9) is mojibake for "café".
        z.start_file("messages/inbox/alice_abc12345/message_1.json", opts).unwrap();
        z.write_all(
            "{\"participants\":[{\"name\":\"Alice\"},{\"name\":\"David\"}],\
             \"title\":\"Alice\",\
             \"messages\":[\
               {\"sender_name\":\"Alice\",\"timestamp_ms\":1700000000000,\
                \"content\":\"Hey David!\",\
                \"reactions\":[{\"reaction\":\"\u{00f0}\u{009f}\u{0098}\u{008d}\",\
                                \"actor\":\"caf\u{00c3}\u{00a9} friend\"}]},\
               {\"sender_name\":\"David\",\"timestamp_ms\":1700000060000,\
                \"content\":\"Hey Alice!\",\
                \"photos\":[{\"uri\":\"messages/inbox/alice_abc12345/photos/photo1.jpg\"}]}\
             ]}"
            .as_bytes(),
        )
        .unwrap();

        // Conversation 1 — shard 2 (older message).
        z.start_file("messages/inbox/alice_abc12345/message_2.json", opts).unwrap();
        z.write_all(
            b"{\"participants\":[{\"name\":\"Alice\"},{\"name\":\"David\"}],\
              \"title\":\"Alice\",\
              \"messages\":[\
                {\"sender_name\":\"Alice\",\"timestamp_ms\":1699900000000,\
                 \"content\":\"old message\"}\
              ]}",
        )
        .unwrap();

        // Conversation 2 — group chat with a share link.
        z.start_file("messages/inbox/groupchat_def67890/message_1.json", opts).unwrap();
        z.write_all(
            b"{\"participants\":[{\"name\":\"Alice\"},{\"name\":\"Bob\"},{\"name\":\"David\"}],\
              \"title\":\"Group Chat\",\
              \"messages\":[\
                {\"sender_name\":\"Bob\",\"timestamp_ms\":1700100000000,\
                 \"share\":{\"link\":\"https://example.com/article\",\
                            \"share_text\":\"check this out\"}}\
              ]}",
        )
        .unwrap();

        // A photo file that must be ignored (not copied into the vault).
        z.start_file("messages/inbox/alice_abc12345/photos/photo1.jpg", opts).unwrap();
        z.write_all(b"\xff\xd8\xff not-a-real-jpeg").unwrap();

        z.finish().unwrap();
        path
    }

    // ------------------------------------------------------------------
    // Tests.

    #[test]
    fn imports_messages_from_both_shards() {
        let v = temp_vault("shards");
        let zip = build_zip("shards");
        let out = run(&v, &zip, "David");

        // 3 messages (2 from shard 1 + 1 from shard 2) + 1 group message = 4,
        // across 2 conversations.
        assert_eq!(out.counts.get("messages"), Some(&4), "{}", out.headline);
        assert_eq!(out.counts.get("conversations"), Some(&2), "{}", out.headline);

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reaction_becomes_correspondence_row_with_mojibake_decoded() {
        let v = temp_vault("reaction");
        let zip = build_zip("reaction");
        let out = run(&v, &zip, "David");

        assert_eq!(out.counts.get("reactions"), Some(&1), "{}", out.headline);

        // The reaction row should be in the 2023-11 month file
        // (1700000000000 ms = 2023-11-14 UTC).
        let month = v
            .root()
            .join("correspondence/facebook-messenger/2023-11.jsonl");
        assert!(month.exists(), "month file should exist");
        let content = fs::read_to_string(&month).unwrap();
        let rows: Vec<Message> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let rx = rows.iter().find(|m| m.kind == "reaction").unwrap();
        // The emoji in the fixture is mojibake for 😍 (U+1F60D).
        // Meta stores 😍 as bytes F0 9F 98 8D mis-read as Latin-1.
        assert_eq!(rx.reaction, "😍", "mojibake reaction emoji decoded: {:?}", rx.reaction);
        // Actor "cafÃ© friend" → "café friend".
        assert_eq!(rx.sender_name, "café friend", "mojibake actor name decoded");

        let _ = fs::remove_file(month);
    }

    #[test]
    fn photo_message_has_attachment_metadata_only() {
        let v = temp_vault("photo");
        let zip = build_zip("photo");
        run(&v, &zip, "David");

        let month = v
            .root()
            .join("correspondence/facebook-messenger/2023-11.jsonl");
        let content = fs::read_to_string(&month).unwrap();
        let rows: Vec<Message> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let photo_msg = rows
            .iter()
            .find(|m| m.kind == "message" && !m.attachments.is_empty() && m.text == "Hey Alice!")
            .expect("photo message not found");
        assert_eq!(photo_msg.attachments[0].name, "photo1.jpg");
        assert_eq!(photo_msg.attachments[0].mime, "image");
        assert_eq!(photo_msg.attachments[0].bytes, 0, "metadata only — bytes never fetched");

        let _ = fs::remove_file(month);
    }

    #[test]
    fn share_link_becomes_attachment_and_sets_text() {
        let v = temp_vault("share");
        let zip = build_zip("share");
        run(&v, &zip, "David");

        // groupchat message is in 2023-11 (1700100000000 ms ~ 2023-11-16 UTC).
        let month = v
            .root()
            .join("correspondence/facebook-messenger/2023-11.jsonl");
        let content = fs::read_to_string(&month).unwrap();
        let rows: Vec<Message> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        // Full folder name is the chat key — hash suffix kept for disambiguation.
        let share_msg = rows
            .iter()
            .find(|m| m.chat == "groupchat_def67890" && m.kind == "message")
            .expect("share message not found");
        assert_eq!(share_msg.text, "check this out");
        let link_att = share_msg.attachments.iter().find(|a| a.mime == "link").unwrap();
        assert_eq!(link_att.name, "https://example.com/article");
        assert_eq!(share_msg.chat_name, "Group Chat");

        let _ = fs::remove_file(month);
    }

    #[test]
    fn from_me_set_correctly_case_insensitive() {
        let v = temp_vault("fromme");
        let zip = build_zip("fromme");
        // Pass lowercase "david" to exercise case-insensitive match.
        run(&v, &zip, "david");

        let month = v
            .root()
            .join("correspondence/facebook-messenger/2023-11.jsonl");
        let content = fs::read_to_string(&month).unwrap();
        let rows: Vec<Message> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let mine = rows.iter().find(|m| m.sender_name == "David" && m.kind == "message").unwrap();
        assert!(mine.from_me, "David's message should be from_me");
        assert!(mine.sender.is_empty(), "sender empty when from_me");

        let theirs = rows
            .iter()
            .find(|m| m.sender_name == "Alice" && m.kind == "message")
            .unwrap();
        assert!(!theirs.from_me);
        assert_eq!(theirs.sender, "Alice");

        let _ = fs::remove_file(month);
    }

    #[test]
    fn mojibake_content_decoded() {
        // "café time 😀" as Meta mojibake in message content.
        // é = UTF-8 C3 A9, mis-read as Latin-1 → U+00C3 U+00A9.
        // 😀 = UTF-8 F0 9F 98 80, mis-read as Latin-1 → U+00F0 U+009F U+0098 U+0080.
        let path = std::env::temp_dir().join(format!(
            "trove-fbm-moji-{}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("messages/inbox/testconv_aabb1122/message_1.json", opts).unwrap();
        // Content "caf\u{c3}\u{a9} time \u{f0}\u{9f}\u{98}\u{80}" is mojibake for "café time 😀"
        let body = "{\"title\":\"Test\",\"participants\":[{\"name\":\"Alice\"}],\
                    \"messages\":[{\"sender_name\":\"Alice\",\"timestamp_ms\":1700000000000,\
                    \"content\":\"caf\u{00c3}\u{00a9} time \u{00f0}\u{009f}\u{0098}\u{0080}\"}]}";
        z.write_all(body.as_bytes()).unwrap();
        z.finish().unwrap();

        let v = temp_vault("moji");
        run(&v, &path, "Me");

        let month = v.root().join("correspondence/facebook-messenger/2023-11.jsonl");
        let content = fs::read_to_string(&month).unwrap();
        assert!(
            content.contains("café time 😀"),
            "mojibake decoded in content: {content}"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn reimport_is_noop() {
        let v = temp_vault("reimport");
        let zip = build_zip("reimport");

        let first = run(&v, &zip, "David");
        let month_path = v.root().join("correspondence/facebook-messenger/2023-11.jsonl");
        let before = fs::read_to_string(&month_path).unwrap_or_default();

        let second = run(&v, &zip, "David");
        let after = fs::read_to_string(&month_path).unwrap_or_default();

        assert_eq!(second.counts.get("messages"), Some(&0), "all messages deduped on re-import");
        assert_eq!(second.counts.get("reactions"), Some(&0), "all reactions deduped on re-import");
        assert!(
            second.counts.get("duplicates").unwrap() >= first.counts.get("messages").unwrap(),
            "duplicates count at least as large as first import's messages"
        );
        assert_eq!(before, after, "vault file unchanged on re-import");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn archived_thread_path_is_parsed() {
        let path = std::env::temp_dir().join(format!(
            "trove-fbm-arch-{}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file(
            "messages/archived_threads/someone_ff001122/message_1.json",
            opts,
        )
        .unwrap();
        z.write_all(
            b"{\"title\":\"Someone\",\"participants\":[{\"name\":\"Someone\"}],\
              \"messages\":[{\"sender_name\":\"Someone\",\"timestamp_ms\":1700000000000,\
              \"content\":\"archived\"}]}",
        )
        .unwrap();
        z.finish().unwrap();

        let v = temp_vault("archived");
        let out = run(&v, &path, "Me");
        assert_eq!(
            out.counts.get("messages"),
            Some(&1),
            "archived thread parsed: {}",
            out.headline
        );

        let _ = fs::remove_file(path);
    }

    /// Full folder name (including the hash suffix) is preserved as the chat key.
    /// Two friends both named "John Smith" get folders `johnsmith_aaaa1111` and
    /// `johnsmith_bbbb2222`; they must stay as SEPARATE chat keys.
    #[test]
    fn conv_folder_used_as_chat_key_verbatim() {
        let v = temp_vault("convkey");
        let path = std::env::temp_dir().join(format!(
            "trove-fbm-convkey-{}.zip",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Two conversations whose display name would collide after hash-stripping.
        z.start_file("messages/inbox/johnsmith_aaaa1111/message_1.json", opts).unwrap();
        z.write_all(
            b"{\"title\":\"John Smith\",\"participants\":[{\"name\":\"John Smith\"}],\
              \"messages\":[{\"sender_name\":\"John Smith\",\"timestamp_ms\":1700000000000,\
              \"content\":\"from john ONE\"}]}",
        ).unwrap();
        z.start_file("messages/inbox/johnsmith_bbbb2222/message_1.json", opts).unwrap();
        z.write_all(
            b"{\"title\":\"John Smith\",\"participants\":[{\"name\":\"John Smith\"}],\
              \"messages\":[{\"sender_name\":\"John Smith\",\"timestamp_ms\":1700000000000,\
              \"content\":\"from john TWO\"}]}",
        ).unwrap();
        z.finish().unwrap();

        let out = run(&v, &path, "Me");
        // Two distinct conversations, both messages imported (no merging, no drops).
        assert_eq!(out.counts.get("conversations"), Some(&2), "two separate convos: {}", out.headline);
        assert_eq!(out.counts.get("messages"), Some(&2), "both messages kept: {}", out.headline);

        let month = v.root().join("correspondence/facebook-messenger/2023-11.jsonl");
        let content = std::fs::read_to_string(&month).unwrap();
        assert!(content.contains("\"johnsmith_aaaa1111\""), "first folder key preserved");
        assert!(content.contains("\"johnsmith_bbbb2222\""), "second folder key preserved");
        assert!(content.contains("from john ONE"), "first message present");
        assert!(content.contains("from john TWO"), "second message present");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn message_guid_injective_and_stable() {
        // Length-prefixing: ("a|b", 0, "c", "") and ("a", 0, "b|c", "") must differ.
        let g1 = message_guid("a|b", 0, "c", "");
        let g2 = message_guid("a", 0, "b|c", "");
        assert_ne!(g1, g2, "component-boundary collision");
        // Stable for same inputs.
        assert_eq!(
            message_guid("chat1", 1700000000000, "Alice", "hello"),
            message_guid("chat1", 1700000000000, "Alice", "hello")
        );
        assert_eq!(message_guid("chat1", 1700000000000, "Alice", "hello").len(), 64);
        // Different ts → different guid.
        assert_ne!(message_guid("chat", 1, "Alice", "hi"), message_guid("chat", 2, "Alice", "hi"));
        // Same ts + sender but different content → different guid (rapid-fire fix).
        assert_ne!(
            message_guid("chat", 1700000000000, "Alice", "msg one"),
            message_guid("chat", 1700000000000, "Alice", "msg two"),
            "same-ms same-sender different-content must not collide"
        );
    }

    #[test]
    fn old_message_lines_still_deserialize() {
        // Back-compat: a sparse correspondence line still deserializes.
        let line = r#"{"ts":"2023-11-14T12:00:00-07:00","source":"facebook-messenger","chat":"alice","from_me":false,"kind":"message","text":"hey"}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hey");
        assert!(!m.from_me);
        assert_eq!(m.source, "facebook-messenger");
    }
}
