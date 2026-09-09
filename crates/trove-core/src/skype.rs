//! Skype (archival) — one-shot import of the official Skype data export.
//!
//! Skype shut down in May 2025. The export (available via secure.skype.com
//! through June 2026) is a `.tar` archive containing a `messages.json` file.
//! This importer accepts either the `.tar` archive or a bare `.json` (for
//! users who extracted it). All messages and call records land in the unified
//! correspondence stream (`correspondence/skype/YYYY-MM.jsonl`).
//!
//! ## Export format (messages.json)
//!
//! ```json
//! {
//!   "userId": "8:live:alice",
//!   "exportDate": "2025-06-01T00:00:00Z",
//!   "conversations": [
//!     {
//!       "id": "8:live:bob",
//!       "displayName": "Bob Smith",
//!       "MessageList": [
//!         {
//!           "id": "1717241000000",
//!           "originalarrivaltime": "2025-06-01T12:00:00.000Z",
//!           "messagetype": "RichText",
//!           "displayName": "Alice",
//!           "from": "8:live:alice",
//!           "content": "Hello!",
//!           "amsreferences": null,
//!           "properties": null
//!         },
//!         {
//!           "id": "1717241100000",
//!           "originalarrivaltime": "2025-06-01T12:01:00.000Z",
//!           "messagetype": "Event/Call",
//!           "displayName": "Alice",
//!           "from": "8:live:alice",
//!           "content": "<partlist type=\"ended\"><part identity=\"8:live:alice\"><name>Alice</name><duration>65</duration></part></partlist>",
//!           "properties": null
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! ### Call records
//!
//! Call state (`ended`, `missed`, `started`) is carried by the `type` attribute
//! on the `<partlist>` element in the `content` XML. There is NO `calltype` or
//! `call_state` key in `properties` in the official export format. Call duration
//! lives in `<duration>` inside the partlist XML. Direction (incoming/outgoing)
//! is inferred from whether the `from` field matches the export owner's `userId`.
//!
//! ### Attachment / media messages
//!
//! `RichText/UriObject`, `RichText/Media_GenericFile`, `RichText/Media_Video`,
//! `RichText/Media_AudioMsg`, `RichText/Contacts`, `RichText/Location` and polls
//! carry their payload in XML attributes inside `content` (e.g. `uri`, `filename`)
//! and often have an empty plain-text body. These are preserved with a placeholder
//! label rather than being silently dropped.
//!
//! ## Vault mapping
//!
//! - Raw layer: `correspondence/skype/raw/messages.json` — the full JSON as
//!   shipped (written from the in-memory parsed value to avoid re-reading the
//!   archive). For `.tar` imports the raw file is extracted as-is.
//! - Contract layer: `correspondence/skype/YYYY-MM.jsonl` per the ratified
//!   correspondence contract.
//!   - `kind:"message"` for chat rows (`RichText`, `RichText/Html`, etc.)
//!   - `kind:"call"` with `duration_secs` for call records (`Event/Call`)
//!   - Thread-activity messages (group renames, member changes) → `kind:"event"`
//!   - `service` = `"Skype"`
//!   - Skype-specific fields (call type, original messagetype) in `extra`
//! - Dedupe key: Skype's numeric message `id` as `guid`; re-importing the same
//!   archive is a no-op.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::correspondence::Message;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Source JSON shapes (messages.json)

#[derive(Debug, Deserialize)]
struct SkypeExport {
    #[serde(rename = "userId", default)]
    user_id: String,
    #[serde(default)]
    conversations: Vec<SkypeConversation>,
}

#[derive(Debug, Deserialize)]
struct SkypeConversation {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: String,
    #[serde(rename = "MessageList", default)]
    message_list: Vec<SkypeMessage>,
}

#[derive(Debug, Deserialize)]
struct SkypeMessage {
    #[serde(default)]
    id: String,
    #[serde(rename = "originalarrivaltime", default)]
    original_arrival_time: String,
    #[serde(rename = "messagetype", default)]
    message_type: String,
    /// Sender display name.
    #[serde(rename = "displayName", default)]
    display_name: String,
    /// Sender Skype ID — format like "8:live:username" or "8:username".
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    content: String,
}

// ---------------------------------------------------------------------------
// Timestamp parsing

/// ISO-8601 / RFC3339 from the export (`"2025-06-01T12:00:00.000Z"`) → local.
fn skype_ts_to_local(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // messages.json uses ISO-8601 with milliseconds: "2025-06-01T12:00:00.000Z"
    // chrono's parse_from_rfc3339 handles the standard form; try with and
    // without fractional seconds and with the space variant.
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Local).to_rfc3339());
    }
    // Some exports have a space separator instead of 'T'.
    let swapped = s.replacen(' ', "T", 1);
    DateTime::parse_from_rfc3339(&swapped)
        .ok()
        .map(|t| t.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Skype user id helpers

/// Extract the bare username from a Skype user-id string like "8:live:alice"
/// or "8:alice". The prefix segment is the resource type; the last segment
/// (after the last colon) is the account name.
fn skype_id_to_handle(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}

// ---------------------------------------------------------------------------
// Call record parsing
//
// Calls come in as messagetype=="Event/Call". The `content` is an XML
// fragment whose root `<partlist>` element carries the call state in its
// `type` attribute: "ended", "missed", or "started". Call duration (seconds)
// lives inside a `<duration>` child element of the partlist.
//
// The `properties` blob in the official export does NOT carry "calltype" or
// "duration" keys; those appear in live-sync API responses, not in the
// messages.json export file.
//
// Direction (incoming / outgoing) is not in the export — it is inferred from
// whether the `from` field matches the export owner's userId (handled at the
// call-site via the `from_me` flag already computed for every message).

/// Extract call duration (seconds) from the content XML (`<duration>N</duration>`).
/// Returns 0 for missed/unanswered calls and when the element is absent.
fn call_duration(content: &str) -> u64 {
    // Scan the content XML for <duration>N</duration>.
    // We avoid pulling in a full XML parser — the field is a simple number.
    if let Some(start) = content.find("<duration>") {
        let rest = &content[start + 10..];
        if let Some(end) = rest.find("</duration>") {
            if let Ok(n) = rest[..end].trim().parse::<u64>() {
                return n;
            }
        }
    }
    0
}

/// Extract call state from the `type` attribute of the `<partlist>` root
/// element in the content XML. Returns `"ended"`, `"missed"`, `"started"`,
/// or `""` when absent/unparseable.
fn call_state_from_content(content: &str) -> &str {
    // Find '<partlist' then look for type="..." within the opening tag.
    let Some(pl_start) = content.find("<partlist") else { return "" };
    let tag_rest = &content[pl_start..];
    // End of the opening tag.
    let tag_end = tag_rest.find('>').unwrap_or(tag_rest.len());
    let opening = &tag_rest[..tag_end];
    // Extract type="value" — handle both single and double quotes.
    for quote in ['"', '\''] {
        let needle = format!("type={quote}");
        if let Some(pos) = opening.find(&needle) {
            let after = &opening[pos + needle.len()..];
            if let Some(end) = after.find(quote) {
                return match &after[..end] {
                    "ended" => "ended",
                    "missed" => "missed",
                    "started" => "started",
                    _ => "",
                };
            }
        }
    }
    ""
}

// ---------------------------------------------------------------------------
// Import stats

#[derive(Debug, Default, Serialize)]
pub struct SkypeImportStats {
    pub messages: u64,
    pub calls: u64,
    pub events: u64,
    pub duplicates: u64,
    pub conversations: u64,
}

// ---------------------------------------------------------------------------
// Core importer

/// Parse a `messages.json` body and ingest into the vault.
fn import_json(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<SkypeImportStats> {
    let export: SkypeExport =
        serde_json::from_str(body).context("parsing Skype messages.json")?;

    let owner_id = export.user_id.clone();
    let owner_handle = skype_id_to_handle(&owner_id).to_string();

    // Build the existing guid set once — cheap, drives idempotent re-import.
    let mut seen: HashSet<String> = vault.correspondence_guids("skype")?;

    let mut batch: Vec<Message> = Vec::new();
    let mut stats = SkypeImportStats::default();
    let conv_count = export.conversations.len() as u64;
    stats.conversations = conv_count;

    for conv in &export.conversations {
        let chat_id = &conv.id;
        // displayName is empty for 1:1 DMs (use the other party's id handle).
        let chat_name = if conv.display_name.is_empty() {
            skype_id_to_handle(chat_id).to_string()
        } else {
            conv.display_name.clone()
        };

        for sm in &conv.message_list {
            if sm.id.is_empty() || sm.original_arrival_time.is_empty() {
                continue;
            }
            let Some(ts) = skype_ts_to_local(&sm.original_arrival_time) else {
                continue;
            };

            let guid = sm.id.clone();
            if !seen.insert(guid.clone()) {
                stats.duplicates += 1;
                continue;
            }

            let sender_id = sm.from.as_deref().unwrap_or("");
            let sender_handle = skype_id_to_handle(sender_id);
            let from_me = !owner_handle.is_empty()
                && (sender_handle.eq_ignore_ascii_case(&owner_handle)
                    || (!owner_id.is_empty() && sender_id == owner_id));

            let mut m = Message::new("skype", ts);
            m.guid = guid;
            m.chat = chat_id.clone();
            m.chat_name = if chat_name == skype_id_to_handle(chat_id) {
                String::new()
            } else {
                chat_name.clone()
            };
            m.service = "Skype".to_string();
            m.from_me = from_me;

            if !from_me {
                // Use displayName from the message as sender name; handle as
                // sender (the Skype ID minus the resource prefix).
                m.sender = sender_handle.to_string();
                if !sm.display_name.is_empty() && sm.display_name != sender_handle {
                    m.sender_name = sm.display_name.clone();
                }
            }

            let mt = sm.message_type.as_str();
            if mt == "Event/Call" {
                // Call record.
                // State (ended/missed/started) comes from the <partlist type="...">
                // attribute in the content XML — there is no "calltype" key in
                // the export's properties blob.
                m.kind = "call".to_string();
                m.duration_secs = call_duration(&sm.content);
                let state = call_state_from_content(&sm.content);
                let direction = if from_me { "outgoing" } else { "incoming" };
                m.text = match state {
                    "missed" => format!("Call (missed, {direction})"),
                    "ended" | "started" => {
                        if m.duration_secs > 0 {
                            format!("Call ({direction}, {}s)", m.duration_secs)
                        } else {
                            format!("Call ({direction})")
                        }
                    }
                    _ => format!("Call ({direction})"),
                };
                stats.calls += 1;
            } else if mt.starts_with("ThreadActivity/") || mt.starts_with("PopCard/") {
                // Group management events — member join/leave, topic change.
                m.kind = "event".to_string();
                m.text = sm.content.clone();
                stats.events += 1;
            } else {
                // Regular message: RichText, RichText/Html, RichText/Media_*, …
                m.kind = "message".to_string();
                // Strip inline markup from all RichText variants — plain RichText
                // can contain <ss> emoticon tags, <a href> links, etc., and
                // RichText/Html contains full HTML. Apply the same tag-stripper
                // uniformly across all subtypes (best-effort; entities are kept
                // verbatim as a fidelity-preserving choice — read-time unescape).
                m.text = if mt.starts_with("RichText") {
                    strip_simple_html(&sm.content)
                } else {
                    sm.content.clone()
                };
                // Media / attachment messages (RichText/UriObject,
                // RichText/Media_GenericFile, RichText/Media_Video,
                // RichText/Media_AudioMsg, RichText/Contacts, RichText/Location,
                // polls, etc.) carry their payload in XML attributes and often
                // have an empty plain-text body. Emit a placeholder rather than
                // silently dropping the row.
                if m.text.is_empty() {
                    m.text = placeholder_for_type(mt).to_string();
                    if m.text.is_empty() {
                        // Truly unknown empty message (e.g. edit placeholder).
                        seen.remove(&m.guid);
                        continue;
                    }
                }
                stats.messages += 1;
            }

            batch.push(m);
            if batch.len() >= 2000 {
                vault.append_messages(&batch)?;
                batch.clear();
            }
        }
    }
    vault.append_messages(&batch)?;

    let total = stats.messages + stats.calls + stats.events;
    progress(ImportProgress { records: total, percent: 100.0 });
    Ok(stats)
}

/// Return a readable placeholder label for media/attachment messagetypes that
/// carry no plain-text body. Returns `""` for messagetypes that should be
/// silently dropped (edit placeholders, unknown empties).
fn placeholder_for_type(messagetype: &str) -> &'static str {
    match messagetype {
        "RichText/UriObject" => "[photo/image]",
        "RichText/Media_GenericFile" => "[file attachment]",
        "RichText/Media_Video" => "[video]",
        "RichText/Media_AudioMsg" => "[voice message]",
        "RichText/Contacts" => "[contact card]",
        "RichText/Location" => "[location]",
        "RichText/SwiftObject" => "[swift card]",
        "RichText" | "RichText/Html" => {
            // Plain RichText/Html with empty body after stripping — treat as
            // an edit/delete placeholder; drop it.
            ""
        }
        _ if messagetype.starts_with("RichText/Media_") => "[media attachment]",
        _ if messagetype.starts_with("RichText/") => "[attachment]",
        _ => "",
    }
}

/// Minimal HTML-tag stripper for RichText content.
/// Strips `<…>` tags uniformly; does NOT unescape HTML entities (kept
/// verbatim for raw fidelity — a read-time unescape concern).
fn strip_simple_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The run_import hook

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let body = if ext == "tar" {
        extract_messages_json_from_tar(path)
            .context("extracting messages.json from Skype .tar archive")?
    } else {
        // Bare .json — read directly (or .tar.gz if somehow extracted to .json).
        std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?
    };

    // Raw layer: write a copy under correspondence/skype/raw/.
    write_raw(vault, &body)?;

    let s = import_json(vault, &body, progress)?;

    Ok(ImportOutcome {
        headline: format!(
            "{} messages, {} calls, {} events imported from {} conversations; {} duplicates skipped",
            s.messages, s.calls, s.events, s.conversations, s.duplicates
        ),
        counts: [
            ("messages", s.messages),
            ("calls", s.calls),
            ("events", s.events),
            ("conversations", s.conversations),
            ("duplicates", s.duplicates),
        ]
        .into(),
    })
}

/// Extract `messages.json` from a Skype `.tar` archive (uncompressed tar —
/// Skype exports are plain tar, not gzip-compressed).
fn extract_messages_json_from_tar(path: &Path) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let entry_path = entry.path().context("entry path")?;
        let name = entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if name.eq_ignore_ascii_case("messages.json") {
            let mut body = String::new();
            entry.read_to_string(&mut body).context("reading messages.json from tar")?;
            return Ok(body);
        }
    }
    anyhow::bail!("no messages.json found inside the Skype tar archive")
}

/// Write the raw messages.json under `correspondence/skype/raw/messages.json`.
/// Overwrites on re-import (idempotent).
fn write_raw(vault: &Vault, body: &str) -> Result<()> {
    let raw_dir = vault.resolve("correspondence/skype/raw")?;
    std::fs::create_dir_all(&raw_dir).context("creating correspondence/skype/raw/")?;
    let raw_path = raw_dir.join("messages.json");
    crate::store::write_atomic(&raw_path, body.as_bytes()).context("writing raw messages.json")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// DEF

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/skype"))
}

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["tar", "json"],
    params: &[],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "skype",
        name: "Skype (archival)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Skype message and call history from the official \
                      data export archive. Skype shut down in May 2025 — exports \
                      were available through June 2026. Drop the .tar archive (or \
                      extracted messages.json) here to preserve two decades of chat \
                      history. Re-runnable and deduplicated.",
        domain: "correspondence",
        vault_path: "correspondence/skype/",
        toggleable: false,
        setup: &[
            "Download your Skype export from secure.skype.com → Account → Export files \
             and chat history (deadline was June 2026 — if you have the archive, import it now).",
            "Drop the .tar archive here. If you already extracted it, drop the messages.json instead.",
        ],
        caveats: "Skype is shut down. This importer only works with a previously downloaded \
                  export archive. After the June 2026 deadline, data that was not exported is \
                  gone forever. Message bodies are imported verbatim — they stay local on your \
                  machine in the vault.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(tag: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-skype-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal messages.json with realistic data matching the actual export
    /// format. Call records use the real `<partlist type="...">` XML shape —
    /// there is NO `calltype` or `duration` key in `properties` in the export.
    fn minimal_json() -> String {
        r#"{
          "userId": "8:live:alice",
          "exportDate": "2025-06-01T00:00:00.000Z",
          "conversations": [
            {
              "id": "8:live:bob",
              "displayName": "Bob Smith",
              "MessageList": [
                {
                  "id": "1717241000000",
                  "originalarrivaltime": "2025-06-01T12:00:00.000Z",
                  "messagetype": "RichText",
                  "displayName": "Bob",
                  "from": "8:live:bob",
                  "content": "Hello Alice!",
                  "properties": null
                },
                {
                  "id": "1717241100000",
                  "originalarrivaltime": "2025-06-01T12:01:40.000Z",
                  "messagetype": "RichText",
                  "displayName": "Alice",
                  "from": "8:live:alice",
                  "content": "Hi Bob!",
                  "properties": null
                },
                {
                  "id": "1717241200000",
                  "originalarrivaltime": "2025-06-01T12:03:00.000Z",
                  "messagetype": "Event/Call",
                  "displayName": "Alice",
                  "from": "8:live:alice",
                  "content": "<partlist type=\"ended\"><part identity=\"8:live:alice\"><name>Alice</name><duration>125</duration></part></partlist>",
                  "properties": null
                },
                {
                  "id": "1717241300000",
                  "originalarrivaltime": "2025-06-01T12:10:00.000Z",
                  "messagetype": "Event/Call",
                  "displayName": "Bob",
                  "from": "8:live:bob",
                  "content": "<partlist type=\"missed\"></partlist>",
                  "properties": null
                },
                {
                  "id": "1717241600000",
                  "originalarrivaltime": "2025-06-01T12:15:00.000Z",
                  "messagetype": "RichText/UriObject",
                  "displayName": "Bob",
                  "from": "8:live:bob",
                  "content": "<URIObject type=\"Picture.1\" uri=\"https://api.asm.skype.com/v1/objects/abc123\"></URIObject>",
                  "properties": null
                }
              ]
            },
            {
              "id": "19:group-abc@thread.skype",
              "displayName": "Team Chat",
              "MessageList": [
                {
                  "id": "1717241400000",
                  "originalarrivaltime": "2025-06-01T13:00:00.000Z",
                  "messagetype": "RichText/Html",
                  "displayName": "Alice",
                  "from": "8:live:alice",
                  "content": "<b>Hello</b> everyone!",
                  "properties": null
                },
                {
                  "id": "1717241500000",
                  "originalarrivaltime": "2025-06-01T13:01:00.000Z",
                  "messagetype": "ThreadActivity/AddMember",
                  "displayName": "Alice",
                  "from": "8:live:alice",
                  "content": "Alice added Carol to the group.",
                  "properties": null
                }
              ]
            }
          ]
        }"#
        .to_string()
    }

    fn run(vault: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(vault, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn ts_parses_iso8601_with_millis() {
        let t = skype_ts_to_local("2025-06-01T12:00:00.000Z").unwrap();
        assert!(t.starts_with("2025-06-01"), "got: {t}");
        // Standard RFC3339.
        let t2 = skype_ts_to_local("2025-06-01T12:00:00+00:00").unwrap();
        assert!(t2.starts_with("2025-06-01"), "got: {t2}");
        assert!(skype_ts_to_local("").is_none());
        assert!(skype_ts_to_local("not-a-date").is_none());
    }

    #[test]
    fn skype_id_extracts_handle() {
        assert_eq!(skype_id_to_handle("8:live:alice"), "alice");
        assert_eq!(skype_id_to_handle("8:alice"), "alice");
        assert_eq!(skype_id_to_handle("alice"), "alice");
    }

    #[test]
    fn call_duration_extracted_from_xml() {
        // Real export shape: duration in <partlist> XML, not in properties.
        assert_eq!(
            call_duration("<partlist type=\"ended\"><part><duration>65</duration></part></partlist>"),
            65
        );
        assert_eq!(
            call_duration("<partlist><part><duration>42</duration></part></partlist>"),
            42
        );
        // Missed call — no duration element → 0.
        assert_eq!(call_duration("<partlist type=\"missed\"></partlist>"), 0);
        // No XML at all → 0.
        assert_eq!(call_duration("no duration here"), 0);
    }

    #[test]
    fn call_state_parsed_from_partlist_attribute() {
        assert_eq!(
            call_state_from_content("<partlist type=\"ended\"><part></part></partlist>"),
            "ended"
        );
        assert_eq!(
            call_state_from_content("<partlist type=\"missed\"></partlist>"),
            "missed"
        );
        assert_eq!(
            call_state_from_content("<partlist type=\"started\"></partlist>"),
            "started"
        );
        // Missing type attribute → empty string.
        assert_eq!(call_state_from_content("<partlist></partlist>"), "");
        // No partlist at all → empty string.
        assert_eq!(call_state_from_content(""), "");
    }

    #[test]
    fn html_stripping() {
        assert_eq!(strip_simple_html("<b>Hello</b> everyone!"), "Hello everyone!");
        assert_eq!(strip_simple_html("plain text"), "plain text");
        assert_eq!(strip_simple_html(""), "");
    }

    #[test]
    fn json_import_counts_and_kinds() {
        let v = temp_vault("json");
        let body = minimal_json();
        let mut stats = SkypeImportStats::default();
        let s = import_json(&v, &body, &mut |p| stats = SkypeImportStats {
            messages: p.records,
            ..Default::default()
        })
        .unwrap();

        // 4 chat messages: Bob's hello, Alice's reply, Bob's photo (UriObject
        // placeholder), group HTML message.
        // 2 calls (ended outgoing + missed incoming) + 1 event.
        assert_eq!(s.messages, 4, "chat messages (incl. media placeholder)");
        assert_eq!(s.calls, 2, "call records");
        assert_eq!(s.events, 1, "thread events");
        assert_eq!(s.duplicates, 0);
        assert_eq!(s.conversations, 2);
    }

    #[test]
    fn from_me_resolved_via_owner_id() {
        let v = temp_vault("fromme");
        let body = minimal_json();
        import_json(&v, &body, &mut |_| {}).unwrap();

        // Messages in 2025-06.
        let path = v.root().join("correspondence/skype/2025-06.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let msgs: Vec<Message> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect();

        // Bob's message: not from_me; sender = "bob", sender_name = "Bob".
        let bob_msg = msgs.iter().find(|m| m.guid == "1717241000000").unwrap();
        assert!(!bob_msg.from_me);
        assert_eq!(bob_msg.sender, "bob");
        assert_eq!(bob_msg.sender_name, "Bob");

        // Alice's message: from_me; sender must be empty.
        let alice_msg = msgs.iter().find(|m| m.guid == "1717241100000").unwrap();
        assert!(alice_msg.from_me);
        assert_eq!(alice_msg.sender, "", "sender empty when from_me");
    }

    #[test]
    fn call_record_kind_and_duration() {
        let v = temp_vault("call");
        let body = minimal_json();
        import_json(&v, &body, &mut |_| {}).unwrap();

        let path = v.root().join("correspondence/skype/2025-06.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let msgs: Vec<Message> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect();

        // Ended outgoing call with 125 s duration (Alice sent it → from_me).
        // State parsed from <partlist type="ended">; direction from from_me.
        let call = msgs.iter().find(|m| m.guid == "1717241200000").unwrap();
        assert_eq!(call.kind, "call");
        assert_eq!(call.duration_secs, 125);
        assert!(call.text.contains("outgoing"), "call text should say outgoing: {}", call.text);
        assert!(call.from_me);

        // Missed incoming call from Bob — duration 0 (no <duration> element).
        // State from <partlist type="missed">; direction from not from_me.
        let missed = msgs.iter().find(|m| m.guid == "1717241300000").unwrap();
        assert_eq!(missed.kind, "call");
        assert_eq!(missed.duration_secs, 0, "missed call has zero duration");
        assert!(missed.text.contains("missed"), "missed call text: {}", missed.text);
        assert!(!missed.from_me, "missed incoming call not from_me");
    }

    #[test]
    fn media_attachment_gets_placeholder_not_dropped() {
        let v = temp_vault("media");
        let body = minimal_json();
        import_json(&v, &body, &mut |_| {}).unwrap();

        let path = v.root().join("correspondence/skype/2025-06.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let msgs: Vec<Message> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect();

        // RichText/UriObject (Bob's photo) should appear as a message row with
        // a placeholder label, not be silently dropped.
        let media = msgs.iter().find(|m| m.guid == "1717241600000").unwrap();
        assert_eq!(media.kind, "message");
        assert!(!media.text.is_empty(), "media message must not have empty text");
        assert!(
            media.text.contains("photo") || media.text.contains("image"),
            "placeholder for UriObject: {}",
            media.text
        );
    }

    #[test]
    fn html_content_stripped_for_richtext_html() {
        let v = temp_vault("html");
        let body = minimal_json();
        import_json(&v, &body, &mut |_| {}).unwrap();

        let path = v.root().join("correspondence/skype/2025-06.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let msgs: Vec<Message> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect();

        let html_msg = msgs.iter().find(|m| m.guid == "1717241400000").unwrap();
        assert_eq!(html_msg.text, "Hello everyone!", "HTML stripped: {}", html_msg.text);
    }

    #[test]
    fn thread_activity_becomes_event_kind() {
        let v = temp_vault("event");
        let body = minimal_json();
        import_json(&v, &body, &mut |_| {}).unwrap();

        let path = v.root().join("correspondence/skype/2025-06.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let msgs: Vec<Message> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect();

        let ev = msgs.iter().find(|m| m.guid == "1717241500000").unwrap();
        assert_eq!(ev.kind, "event");
        assert!(ev.text.contains("Carol"), "event text: {}", ev.text);
    }

    #[test]
    fn reimport_is_noop() {
        let v = temp_vault("reimport");
        let body = minimal_json();
        let s1 = import_json(&v, &body, &mut |_| {}).unwrap();
        let s2 = import_json(&v, &body, &mut |_| {}).unwrap();
        assert_eq!(s2.messages, 0, "second import adds no messages");
        assert_eq!(s2.calls, 0, "second import adds no calls");
        assert_eq!(s2.duplicates, s1.messages + s1.calls + s1.events);
    }

    #[test]
    fn json_file_import_via_importspec() {
        let v = temp_vault("importspec");
        let json_path = std::env::temp_dir().join(format!(
            "trove-skype-test-{}.json",
            std::process::id()
        ));
        fs::write(&json_path, minimal_json()).unwrap();

        let out = run(&v, &json_path);
        assert_eq!(out.counts.get("messages"), Some(&4));
        assert_eq!(out.counts.get("calls"), Some(&2));
        assert_eq!(out.counts.get("events"), Some(&1));
        // Raw file written.
        assert!(v.root().join("correspondence/skype/raw/messages.json").exists());

        let _ = fs::remove_file(&json_path);
    }

    #[test]
    fn tar_file_import_extracts_messages_json() {
        let v = temp_vault("tar");
        // Build a minimal tar in memory containing messages.json.
        let tar_path =
            std::env::temp_dir().join(format!("trove-skype-test-{}.tar", std::process::id()));
        {
            let f = fs::File::create(&tar_path).unwrap();
            let mut ar = tar::Builder::new(f);
            let body = minimal_json();
            let bytes = body.as_bytes();
            let mut header = tar::Header::new_gnu();
            header.set_path("messages.json").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            ar.append(&header, bytes).unwrap();
            ar.finish().unwrap();
        }

        let out = run(&v, &tar_path);
        assert_eq!(out.counts.get("messages"), Some(&4), "tar import counts: {:?}", out.counts);
        assert_eq!(out.counts.get("calls"), Some(&2));

        let _ = fs::remove_file(&tar_path);
    }

    #[test]
    fn old_correspondence_line_still_deserializes() {
        // Back-compat check: a minimal correspondence line without skype-specific
        // fields still deserialises as a Message (serde defaults).
        let line = r#"{"ts":"2025-06-01T12:00:00-07:00","source":"skype","chat":"8:live:bob","from_me":false,"kind":"message","text":"Hello Alice!"}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "Hello Alice!");
        assert!(!m.from_me);
    }
}
