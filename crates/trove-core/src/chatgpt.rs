//! ChatGPT — file import of the official OpenAI conversation export ZIP.
//!
//! OpenAI's account export (Settings → Data Controls → Export) delivers a ZIP
//! containing `conversations.json`: an array of conversations, each with a
//! `mapping` dict (`node_id → {message, parent, children}`). This module
//! walks the tree to linearize messages, writes full-fidelity rows to
//! `developer/chatgpt/YYYY-MM.jsonl` (month-partitioned by message ts), and
//! dedupes by `guid = conversation_id|message_id` so re-importing a newer
//! export only appends new messages.
//!
//! **`developer/` is raw-only** (taxonomy decision): no contract, no
//! normalized struct — this module owns its row shape.
//!
//! ## Privacy gate
//!
//! Content is truncated to [`PREVIEW_LEN`] bytes by default (matching the
//! Claude Code lean-default pattern). Role and model metadata are always
//! stored; the truncated preview makes the data useful for timeline and
//! search without storing full conversation text in the vault by default.
//!
//! ## Access
//!
//! ChatGPT Settings → Data Controls → Export → emailed ZIP. Up to 7 days to
//! prepare; download link expires in 24 hours. No API, no network at import
//! time. Business/Enterprise accounts have no export.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "developer/chatgpt";

/// Truncate content previews to this many bytes by default (privacy-lean).
const PREVIEW_LEN: usize = 280;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "chatgpt",
        name: "ChatGPT",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your ChatGPT conversation history from the official \
                      OpenAI data export. Stores session metadata and truncated \
                      previews; full transcript text is opt-in.",
        domain: "developer",
        vault_path: "developer/chatgpt/",
        toggleable: false,
        setup: &[
            "chatgpt.com → Settings → Data Controls → Export data — an email arrives with a download link.",
            "The export can take up to 7 days to prepare. The download link expires after 24 hours.",
            "Unzip and import conversations.json, or import the ZIP as-is.",
            "Business and Enterprise accounts do not support the data export feature.",
        ],
        caveats: "Export requests can take up to 7 days to prepare and the \
                  download link expires after 24 hours — plan imports accordingly. \
                  Message content is stored as a short preview by default; full text \
                  storage is an opt-in for privacy reasons.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "json"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Raw JSON shapes from conversations.json.

/// Top-level conversation object in `conversations.json`.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ConvRaw {
    id: String,
    #[serde(default)]
    title: Option<String>,
    /// Unix timestamps (f64 from OpenAI). Present in the export; message-level
    /// `create_time` is used for the per-row ts.
    #[serde(default)]
    create_time: Option<f64>,
    #[serde(default)]
    update_time: Option<f64>,
    /// Node dict: node_id → node.
    #[serde(default)]
    mapping: HashMap<String, NodeRaw>,
}

/// One node in the mapping tree.
#[derive(Debug, Deserialize)]
struct NodeRaw {
    id: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    children: Vec<String>,
    /// Absent on root/system-prompt nodes.
    #[serde(default)]
    message: Option<MessageRaw>,
}

/// The message object inside a node.
#[derive(Debug, Deserialize)]
struct MessageRaw {
    id: String,
    #[serde(default)]
    author: AuthorRaw,
    /// Unix timestamp (f64). Missing on some system nodes.
    #[serde(default)]
    create_time: Option<f64>,
    #[serde(default)]
    content: ContentRaw,
    /// Model slug lives in metadata.model_slug (assistant turns only).
    #[serde(default)]
    metadata: Value,
}

#[derive(Debug, Default, Deserialize)]
struct AuthorRaw {
    #[serde(default)]
    role: String,
    /// Present on tool/browser turns (e.g. "browser", "python", "dalle").
    #[serde(default)]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct ContentRaw {
    #[serde(default)]
    content_type: String,
    /// Parts can be strings or objects (image/code blocks — tolerate both).
    /// Present on: "text", "multimodal_text".
    #[serde(default)]
    parts: Vec<Value>,
    /// Present on: "code" (inline code block), "execution_output", "tether_quote",
    /// "tether_browsing_display", "tether_browsing_code".
    #[serde(default)]
    text: Option<String>,
    /// Present on: "reasoning_recap" (accumulated reasoning text).
    #[serde(default)]
    content: Option<String>,
    /// Present on: "execution_output" (alternate field name in some export versions).
    #[serde(default)]
    result: Option<String>,
    /// Present on: "thoughts" (extended reasoning, o1/o3).
    #[serde(default)]
    thoughts: Option<Value>,
    /// Language tag present on "code" blocks (preserved for raw fidelity;
    /// not currently surfaced in the preview).
    #[allow(dead_code)]
    #[serde(default)]
    language: Option<String>,
}

// ---------------------------------------------------------------------------
// Vault row (raw, this module's own shape — no contract).

/// One row in `developer/chatgpt/YYYY-MM.jsonl`: a single ChatGPT message
/// with conversation metadata. Content is truncated to [`PREVIEW_LEN`] bytes
/// by default for privacy; the full `content_type` and `role` are always
/// stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptRow {
    /// `guid = "{conversation_id}|{message_id}"` — stable dedup key.
    pub guid: String,
    /// RFC3339 local time (from `message.create_time`).
    pub ts: String,
    /// Source tag.
    pub source: String,
    /// Conversation UUID.
    pub conversation_id: String,
    /// Conversation title (empty string when absent).
    pub conversation_title: String,
    /// Message UUID.
    pub message_id: String,
    /// `author.role`: "user" / "assistant" / "system" / "tool".
    pub role: String,
    /// `content.content_type`: "text" / "code" / "multimodal_text" etc.
    pub content_type: String,
    /// Truncated text preview (up to [`PREVIEW_LEN`] bytes, UTF-8-safe).
    /// Empty for non-text content (images, structured data).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_preview: String,
    /// `metadata.model_slug`, e.g. "gpt-4o" (omitted when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Extra fields from the raw message for full-fidelity overflow.
    /// Captures: all `metadata` fields (except `model_slug`, already promoted),
    /// plus message-level fields present in real exports (`recipient`,
    /// `channel`, `status`, `end_turn`, `weight`, `author.name` for tool
    /// turns such as `browser` / `python`).
    /// Does NOT capture the content object (content fields are already handled
    /// by `content_text` / `content_type` / `content_preview`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Import.

/// Read `conversations.json` body: extract from a ZIP, or read directly.
fn conversations_json(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut entry = archive
            .by_name("conversations.json")
            .context("no conversations.json in the export zip — is this a ChatGPT data export?")?;
        let mut body = String::new();
        entry.read_to_string(&mut body).context("reading conversations.json")?;
        Ok(body)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("opening {}", path.display()))
    }
}

/// Unix f64 → RFC3339 local-time string. Returns `None` when the timestamp is
/// missing, zero, or not finite (some system nodes have `create_time: 0.0`).
fn unix_f64_to_rfc3339(ts: Option<f64>) -> Option<String> {
    let t = ts?;
    if !t.is_finite() || t <= 0.0 {
        return None;
    }
    let secs = t as i64;
    let nanos = ((t - secs as f64) * 1_000_000_000.0).round() as u32;
    let dt = DateTime::from_timestamp(secs, nanos)?;
    Some(DateTime::<Local>::from(dt).to_rfc3339())
}

/// Extract the best available text from a content block.
///
/// Content layout varies by `content_type`:
/// - `text` / `multimodal_text`: string items in `parts`
/// - `code`: `content.text` (the code body); may also carry `language`
/// - `execution_output`: `content.text` or `content.result`
/// - `tether_quote` / `tether_browsing_*`: `content.text` (web citation body)
/// - `reasoning_recap` / `thoughts`: `content.content` or the nested
///   `thoughts[].content` array (o1/o3 reasoning traces)
///
/// Returns an empty string when no text is recoverable (e.g., image-only
/// multimodal blocks).
fn content_text(c: &ContentRaw) -> String {
    // Primary: string items in parts (covers "text", "multimodal_text").
    let parts_str: String = c
        .parts
        .iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("");
    if !parts_str.is_empty() {
        return parts_str;
    }

    // Fallback 1: direct `text` field (code, execution_output, tether_*).
    if let Some(t) = &c.text {
        if !t.is_empty() {
            return t.clone();
        }
    }

    // Fallback 2: `content` field (reasoning_recap).
    if let Some(t) = &c.content {
        if !t.is_empty() {
            return t.clone();
        }
    }

    // Fallback 3: `result` field (execution_output alternate).
    if let Some(t) = &c.result {
        if !t.is_empty() {
            return t.clone();
        }
    }

    // Fallback 4: `thoughts` array — concatenate the `content` field of each
    // thought object (o1/o3 reasoning traces).
    if let Some(thoughts) = &c.thoughts {
        let joined: String = thoughts
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("content").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if !joined.is_empty() {
            return joined;
        }
    }

    String::new()
}

/// Truncate a string to at most `max_bytes` UTF-8 bytes, never splitting a
/// multi-byte codepoint.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Walk the mapping tree in conversation order (root → children DFS), emit
/// one [`ChatGptRow`] per message that has a valid timestamp. Messages with
/// `create_time <= 0` or missing are skipped (system scaffold nodes).
fn linearize_conversation(conv: &ConvRaw, seen: &mut HashSet<String>) -> Vec<ChatGptRow> {
    let mut out = Vec::new();

    // Compute a conversation-level model fallback by scanning all nodes for any
    // metadata.model_slug. Production exports often have many assistant messages
    // without a per-message slug; the conversation's model is still recoverable
    // this way (matches the behavior of chatgpt-exporter's extractModel).
    let conv_model: Option<String> = conv
        .mapping
        .values()
        .filter_map(|n| n.message.as_ref())
        .filter_map(|m| {
            m.metadata
                .get("model_slug")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .next();

    // Find root nodes: nodes with no parent, or whose parent is missing from
    // the mapping (the sentinel "client-created root" nodes OpenAI emits).
    let root_ids: Vec<&str> = conv
        .mapping
        .values()
        .filter(|n| {
            n.parent.as_deref().map_or(true, |p| p.is_empty() || !conv.mapping.contains_key(p))
        })
        .map(|n| n.id.as_str())
        .collect();

    // DFS from each root, depth-first following `children`. When a node has
    // multiple children (a branched conversation), we follow all branches so
    // no messages are silently dropped.
    let mut stack: Vec<&str> = root_ids;
    // Guard against malformed cycles.
    let mut visited: HashSet<&str> = HashSet::new();

    while let Some(node_id) = stack.pop() {
        if !visited.insert(node_id) {
            continue;
        }
        let Some(node) = conv.mapping.get(node_id) else { continue };

        // Push children in reverse so the first child is processed first.
        for child_id in node.children.iter().rev() {
            stack.push(child_id.as_str());
        }

        let Some(msg) = &node.message else { continue };

        // Skip nodes with no usable timestamp.
        let Some(ts) = unix_f64_to_rfc3339(msg.create_time) else { continue };

        let guid = format!("{}|{}", conv.id, msg.id);
        if !seen.insert(guid.clone()) {
            continue;
        }

        // Per-message model slug; fall back to conversation-level when absent
        // (covers the common case where only the first/last assistant turn
        // carries model_slug in the real export).
        let model = msg
            .metadata
            .get("model_slug")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| conv_model.clone());

        let full_text = content_text(&msg.content);
        let content_preview = truncate_utf8(&full_text, PREVIEW_LEN).to_string();

        // Extra: overflow of metadata fields (excluding model_slug, already
        // promoted) plus message-level fields present in real exports.
        let mut extra = Map::new();

        // Message-level fields: recipient, channel, status, end_turn, weight.
        // These appear on assistant/tool turns in real exports.
        // We store them under their own key rather than under "metadata" to
        // make the extra map flat and easy to query.
        for key in &["recipient", "channel", "status", "end_turn", "weight"] {
            if let Some(v) = msg.metadata.get(*key) {
                // model_slug is already in msg.metadata; only non-slug fields here.
                // These message-level-like fields are actually in metadata in OpenAI's
                // format, so we pick them from there.
                extra.insert((*key).to_string(), v.clone());
            }
        }
        // author.name is present for tool turns (e.g. "browser", "python").
        if !msg.author.name.is_empty() {
            extra.insert("author_name".to_string(), Value::String(msg.author.name.clone()));
        }

        // All remaining metadata fields (except model_slug).
        if let Value::Object(meta) = &msg.metadata {
            for (k, v) in meta {
                if k != "model_slug" && !extra.contains_key(k.as_str()) {
                    extra.insert(k.clone(), v.clone());
                }
            }
        }

        out.push(ChatGptRow {
            guid,
            ts,
            source: "chatgpt".into(),
            conversation_id: conv.id.clone(),
            conversation_title: conv.title.clone().unwrap_or_default(),
            message_id: msg.id.clone(),
            role: msg.author.role.clone(),
            content_type: msg.content.content_type.clone(),
            content_preview,
            model,
            extra,
        });
    }

    out
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = conversations_json(path)?;
    let conversations: Vec<ConvRaw> =
        serde_json::from_str(&body).context("parsing conversations.json")?;

    let stream = vault.stream(DIR, Partition::Month);

    // Load already-stored guids for dedupe across re-imports.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for row in stream.read::<ChatGptRow>(&key)? {
            if !row.guid.is_empty() {
                seen.insert(row.guid);
            }
        }
    }

    let (mut imported, mut skipped_convs) = (0u64, 0u64);
    let total = conversations.len() as u64;
    let mut rows = Vec::new();

    for (i, conv) in conversations.iter().enumerate() {
        let messages = linearize_conversation(conv, &mut seen);
        if messages.is_empty() {
            skipped_convs += 1;
        } else {
            imported += messages.len() as u64;
            rows.extend(messages);
        }
        if (i + 1) % 50 == 0 || i + 1 == conversations.len() {
            let pct = ((i + 1) as f32 / total.max(1) as f32) * 90.0;
            progress(ImportProgress { records: imported, percent: pct });
        }
    }

    // linearize_conversation already dedupes via the `seen` set (populated from
    // the vault at the top of this fn), so rows here are all new. Conversations
    // with zero timestamped messages are counted in skipped_convs.
    stream.append(&rows, |r| r.ts.as_str())?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{imported} messages imported, {skipped_convs} conversations skipped (no timestamped messages)"
        ),
        counts: [("imported", imported), ("conversations_skipped", skipped_convs)]
            .into_iter()
            .collect(),
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
            std::env::temp_dir().join(format!("trove-chatgpt-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A minimal conversations.json with two conversations:
    /// - conv1: user + assistant turns (both timestamped)
    /// - conv2: only a system node with create_time=0 → should be skipped
    ///
    /// Tests: parsing, tree linearization, ts conversion, role, model_slug,
    /// content_preview truncation, dedup, skipped-conv counting.
    const CONVERSATIONS_JSON: &str = r#"[
  {
    "id": "conv-aaa",
    "title": "Fix the bug",
    "create_time": 1717200000.0,
    "update_time": 1717200120.0,
    "mapping": {
      "root-node": {
        "id": "root-node",
        "parent": null,
        "children": ["msg-user-1"],
        "message": null
      },
      "msg-user-1": {
        "id": "msg-user-1",
        "parent": "root-node",
        "children": ["msg-asst-1"],
        "message": {
          "id": "msg-user-1",
          "author": { "role": "user" },
          "create_time": 1717200010.5,
          "content": {
            "content_type": "text",
            "parts": ["Can you help me fix the null pointer exception?"]
          },
          "metadata": {}
        }
      },
      "msg-asst-1": {
        "id": "msg-asst-1",
        "parent": "msg-user-1",
        "children": [],
        "message": {
          "id": "msg-asst-1",
          "author": { "role": "assistant" },
          "create_time": 1717200030.0,
          "content": {
            "content_type": "text",
            "parts": ["Sure! The null pointer exception typically occurs when..."]
          },
          "metadata": { "model_slug": "gpt-4o" }
        }
      }
    }
  },
  {
    "id": "conv-bbb",
    "title": "System only",
    "create_time": 1717100000.0,
    "update_time": 1717100001.0,
    "mapping": {
      "sys-node": {
        "id": "sys-node",
        "parent": null,
        "children": [],
        "message": {
          "id": "sys-node",
          "author": { "role": "system" },
          "create_time": 0.0,
          "content": { "content_type": "text", "parts": ["You are a helpful assistant."] },
          "metadata": {}
        }
      }
    }
  },
  {
    "id": "conv-ccc",
    "title": "Tool use conversation",
    "create_time": 1717300000.0,
    "update_time": 1717300200.0,
    "mapping": {
      "root-ccc": {
        "id": "root-ccc",
        "parent": null,
        "children": ["msg-tool-call"],
        "message": null
      },
      "msg-tool-call": {
        "id": "msg-tool-call",
        "parent": "root-ccc",
        "children": [],
        "message": {
          "id": "msg-tool-call",
          "author": { "role": "tool" },
          "create_time": 1717300050.0,
          "content": {
            "content_type": "text",
            "parts": ["TOOL_RESULT_DATA"]
          },
          "metadata": { "model_slug": "gpt-4o-mini", "finish_reason": "stop" }
        }
      }
    }
  }
]"#;

    fn do_import(v: &Vault, json_body: &str) -> ImportOutcome {
        let path = v.root().join("conversations.json");
        fs::write(&path, json_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn basic_import_parses_roles_model_and_skips_zerots() {
        let v = temp_vault("basic");
        let out = do_import(&v, CONVERSATIONS_JSON);

        // conv-aaa: 2 messages (user + assistant)
        // conv-bbb: 0 messages (create_time=0 → skipped)
        // conv-ccc: 1 message (tool role)
        // Total: 3 imported, 1 conversation_skipped (conv-bbb)
        assert_eq!(out.counts["imported"], 3, "headline: {}", out.headline);
        assert_eq!(out.counts["conversations_skipped"], 1, "conv-bbb skipped");

        // Check the partition file exists.
        let dir = v.root().join(DIR);
        let mut parts: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        parts.sort();
        assert!(!parts.is_empty(), "at least one partition written");

        // Read rows and verify fields.
        let stream = v.stream(DIR, Partition::Month);
        let mut all_rows: Vec<ChatGptRow> = Vec::new();
        for key in stream.partitions().unwrap() {
            all_rows.extend(stream.read::<ChatGptRow>(&key).unwrap());
        }
        assert_eq!(all_rows.len(), 3);

        let user_row = all_rows.iter().find(|r| r.role == "user").expect("user row");
        assert_eq!(user_row.conversation_id, "conv-aaa");
        assert_eq!(user_row.conversation_title, "Fix the bug");
        assert!(user_row.ts.starts_with("2024-"), "ts in 2024: {}", user_row.ts);
        assert_eq!(user_row.content_type, "text");
        assert!(user_row.content_preview.contains("null pointer exception"));
        // User turns don't carry model_slug per-message, but the conversation-level
        // fallback (from the assistant turn in the same conversation) fills it in.
        // This is the correct behavior: we know what model the user was interacting with.
        assert_eq!(
            user_row.model.as_deref(),
            Some("gpt-4o"),
            "user turn gets conversation-level model fallback"
        );
        assert_eq!(user_row.source, "chatgpt");

        let asst_row = all_rows.iter().find(|r| r.role == "assistant").expect("assistant row");
        assert_eq!(asst_row.model.as_deref(), Some("gpt-4o"));
        assert_eq!(asst_row.guid, "conv-aaa|msg-asst-1");

        let tool_row = all_rows.iter().find(|r| r.role == "tool").expect("tool row");
        assert_eq!(tool_row.model.as_deref(), Some("gpt-4o-mini"));
        // finish_reason lands in extra (not model_slug)
        assert!(tool_row.extra.contains_key("finish_reason"));
    }

    #[test]
    fn reimport_is_idempotent_and_new_messages_append() {
        let v = temp_vault("dedup");

        // First import.
        let out1 = do_import(&v, CONVERSATIONS_JSON);
        assert_eq!(out1.counts["imported"], 3);

        // Re-import the same data — 0 new rows (all guids seen).
        let out2 = do_import(&v, CONVERSATIONS_JSON);
        assert_eq!(out2.counts["imported"], 0, "re-import adds nothing: {}", out2.headline);

        // Extend with one new conversation.
        let extended = format!(
            r#"{}
,[{{
  "id": "conv-ddd",
  "title": "New chat",
  "create_time": 1717400000.0,
  "update_time": 1717400050.0,
  "mapping": {{
    "r": {{
      "id": "r",
      "parent": null,
      "children": ["m1"],
      "message": null
    }},
    "m1": {{
      "id": "m1",
      "parent": "r",
      "children": [],
      "message": {{
        "id": "m1",
        "author": {{ "role": "user" }},
        "create_time": 1717400010.0,
        "content": {{ "content_type": "text", "parts": ["New question"] }},
        "metadata": {{}}
      }}
    }}
  }}
}}]"#,
            // Replace the closing ] of the original with nothing — build a new array.
            "",
        );
        // Build a clean new export that is a superset (original 3 + 1 new).
        let new_export: serde_json::Value =
            serde_json::from_str(CONVERSATIONS_JSON).unwrap();
        let mut arr = new_export.as_array().unwrap().clone();
        let extra: serde_json::Value = serde_json::from_str(r#"{
          "id": "conv-ddd",
          "title": "New chat",
          "create_time": 1717400000.0,
          "update_time": 1717400050.0,
          "mapping": {
            "r": {
              "id": "r",
              "parent": null,
              "children": ["m1"],
              "message": null
            },
            "m1": {
              "id": "m1",
              "parent": "r",
              "children": [],
              "message": {
                "id": "m1",
                "author": { "role": "user" },
                "create_time": 1717400010.0,
                "content": { "content_type": "text", "parts": ["New question"] },
                "metadata": {}
              }
            }
          }
        }"#).unwrap();
        arr.push(extra);
        let new_json = serde_json::to_string(&arr).unwrap();
        let out3 = do_import(&v, &new_json);
        assert_eq!(out3.counts["imported"], 1, "only the new message appended: {}", out3.headline);

        // Total rows should now be 4.
        let stream = v.stream(DIR, Partition::Month);
        let mut total = 0usize;
        for key in stream.partitions().unwrap() {
            total += stream.read::<ChatGptRow>(&key).unwrap().len();
        }
        assert_eq!(total, 4, "4 total rows after incremental import");
        let _ = extended;
    }

    #[test]
    fn imports_from_zip() {
        use std::io::Write;
        let v = temp_vault("zip");
        let zip_path = v.root().join("chatgpt-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("conversations.json", opts).unwrap();
        w.write_all(CONVERSATIONS_JSON.as_bytes()).unwrap();
        // Decoy — must not be read.
        w.start_file("chat.html", opts).unwrap();
        w.write_all(b"<html></html>").unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 3, "zip import: {}", out.headline);
    }

    #[test]
    fn content_preview_truncates_at_280_bytes() {
        // Construct a message with content > 280 bytes.
        let long_text: String = "A".repeat(500);
        let json = format!(
            r#"[{{
          "id": "conv-long",
          "title": "Long",
          "create_time": 1717200000.0,
          "update_time": 1717200100.0,
          "mapping": {{
            "r": {{
              "id": "r", "parent": null, "children": ["m1"],
              "message": null
            }},
            "m1": {{
              "id": "m1", "parent": "r", "children": [],
              "message": {{
                "id": "m1",
                "author": {{"role": "user"}},
                "create_time": 1717200010.0,
                "content": {{"content_type": "text", "parts": ["{long_text}"]}},
                "metadata": {{}}
              }}
            }}
          }}
        }}]"#
        );
        let v = temp_vault("truncate");
        do_import(&v, &json);

        let stream = v.stream(DIR, Partition::Month);
        let mut rows: Vec<ChatGptRow> = Vec::new();
        for key in stream.partitions().unwrap() {
            rows.extend(stream.read::<ChatGptRow>(&key).unwrap());
        }
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].content_preview.len() <= PREVIEW_LEN,
            "preview truncated: {} bytes",
            rows[0].content_preview.len()
        );
        assert_eq!(rows[0].content_preview.len(), PREVIEW_LEN, "exactly at limit");
    }

    #[test]
    fn guid_dedup_format() {
        let mut seen = HashSet::new();
        let conv: ConvRaw = serde_json::from_str(r#"{
          "id": "conv-x",
          "title": "Test",
          "create_time": 1717200000.0,
          "update_time": 1717200100.0,
          "mapping": {
            "r": {
              "id": "r", "parent": null, "children": ["m1"],
              "message": null
            },
            "m1": {
              "id": "m1", "parent": "r", "children": [],
              "message": {
                "id": "m1",
                "author": {"role": "user"},
                "create_time": 1717200010.0,
                "content": {"content_type": "text", "parts": ["hello"]},
                "metadata": {}
              }
            }
          }
        }"#).unwrap();
        let rows = linearize_conversation(&conv, &mut seen);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].guid, "conv-x|m1");

        // Second call with same seen set: deduplicated.
        let rows2 = linearize_conversation(&conv, &mut seen);
        assert_eq!(rows2.len(), 0, "same guid deduplicated");
    }

    #[test]
    fn truncate_utf8_never_splits_codepoint() {
        let s = "héllo"; // 'é' is 2 bytes → len=6
        // Requesting 2 bytes — the boundary is between 'h' and 'é'; 'é' starts at
        // byte 1, so byte 2 is the second byte of 'é' (not a boundary). Must not
        // split.
        let t = truncate_utf8(s, 2);
        assert!(s.is_char_boundary(t.len()), "valid UTF-8 boundary");
        assert_eq!(t, "h", "only the ASCII char before the 2-byte sequence");
    }

    /// Verify that non-"text" content_types with text in alternate fields are
    /// NOT silently zeroed out. Covers the major defect: code/execution_output/
    /// tether_quote/reasoning_recap all store their body outside `parts`.
    #[test]
    fn alternate_content_types_produce_non_empty_preview() {
        let json = r#"[{
          "id": "conv-alt",
          "title": "Alternate content types",
          "create_time": 1717200000.0,
          "update_time": 1717200500.0,
          "mapping": {
            "root": {
              "id": "root", "parent": null,
              "children": ["m-code", "m-exec", "m-tether", "m-recap", "m-thoughts"],
              "message": null
            },
            "m-code": {
              "id": "m-code", "parent": "root", "children": [],
              "message": {
                "id": "m-code",
                "author": {"role": "assistant"},
                "create_time": 1717200010.0,
                "content": {
                  "content_type": "code",
                  "language": "python",
                  "text": "print('hello world')",
                  "parts": []
                },
                "metadata": {"model_slug": "gpt-4o"}
              }
            },
            "m-exec": {
              "id": "m-exec", "parent": "root", "children": [],
              "message": {
                "id": "m-exec",
                "author": {"role": "tool", "name": "python"},
                "create_time": 1717200020.0,
                "content": {
                  "content_type": "execution_output",
                  "text": "hello world\n",
                  "parts": []
                },
                "metadata": {}
              }
            },
            "m-tether": {
              "id": "m-tether", "parent": "root", "children": [],
              "message": {
                "id": "m-tether",
                "author": {"role": "tool", "name": "browser"},
                "create_time": 1717200030.0,
                "content": {
                  "content_type": "tether_quote",
                  "text": "According to the article, the answer is 42.",
                  "parts": []
                },
                "metadata": {}
              }
            },
            "m-recap": {
              "id": "m-recap", "parent": "root", "children": [],
              "message": {
                "id": "m-recap",
                "author": {"role": "assistant"},
                "create_time": 1717200040.0,
                "content": {
                  "content_type": "reasoning_recap",
                  "content": "I need to think about this carefully before answering.",
                  "parts": []
                },
                "metadata": {}
              }
            },
            "m-thoughts": {
              "id": "m-thoughts", "parent": "root", "children": [],
              "message": {
                "id": "m-thoughts",
                "author": {"role": "assistant"},
                "create_time": 1717200050.0,
                "content": {
                  "content_type": "thoughts",
                  "thoughts": [
                    {"content": "First thought here."},
                    {"content": "Second thought here."}
                  ],
                  "parts": []
                },
                "metadata": {}
              }
            }
          }
        }]"#;

        let v = temp_vault("alt-content");
        let out = do_import(&v, json);
        assert_eq!(out.counts["imported"], 5, "all 5 alt-type messages imported");

        let stream = v.stream(DIR, Partition::Month);
        let mut rows: Vec<ChatGptRow> = Vec::new();
        for key in stream.partitions().unwrap() {
            rows.extend(stream.read::<ChatGptRow>(&key).unwrap());
        }
        assert_eq!(rows.len(), 5);

        let code_row = rows.iter().find(|r| r.content_type == "code").expect("code row");
        assert!(
            !code_row.content_preview.is_empty(),
            "code content_preview must not be empty"
        );
        assert!(
            code_row.content_preview.contains("print"),
            "code preview: '{}'",
            code_row.content_preview
        );

        let exec_row =
            rows.iter().find(|r| r.content_type == "execution_output").expect("exec row");
        assert!(
            !exec_row.content_preview.is_empty(),
            "execution_output content_preview must not be empty"
        );
        assert!(exec_row.content_preview.contains("hello world"), "exec preview");
        // author.name for tool turns should appear in extra
        assert_eq!(
            exec_row.extra.get("author_name").and_then(Value::as_str),
            Some("python"),
            "author_name captured for tool turns"
        );

        let tether_row =
            rows.iter().find(|r| r.content_type == "tether_quote").expect("tether row");
        assert!(
            !tether_row.content_preview.is_empty(),
            "tether_quote content_preview must not be empty"
        );
        assert!(tether_row.content_preview.contains("42"), "tether preview");

        let recap_row =
            rows.iter().find(|r| r.content_type == "reasoning_recap").expect("recap row");
        assert!(
            !recap_row.content_preview.is_empty(),
            "reasoning_recap content_preview must not be empty"
        );
        assert!(recap_row.content_preview.contains("carefully"), "recap preview");

        let thoughts_row =
            rows.iter().find(|r| r.content_type == "thoughts").expect("thoughts row");
        assert!(
            !thoughts_row.content_preview.is_empty(),
            "thoughts content_preview must not be empty"
        );
        assert!(
            thoughts_row.content_preview.contains("First thought"),
            "thoughts preview: '{}'",
            thoughts_row.content_preview
        );
    }

    /// Verify that model_slug is carried forward from other messages in the
    /// conversation when the specific message lacks it (the common real-export
    /// case: only a few messages have model_slug in their metadata).
    #[test]
    fn model_slug_fallback_from_conversation() {
        // Two assistant messages, but only the SECOND has model_slug.
        // The first should inherit the conversation-level fallback.
        let json = r#"[{
          "id": "conv-model",
          "title": "Model fallback test",
          "create_time": 1717200000.0,
          "update_time": 1717200200.0,
          "mapping": {
            "root": {
              "id": "root", "parent": null, "children": ["m1"],
              "message": null
            },
            "m1": {
              "id": "m1", "parent": "root", "children": ["m2"],
              "message": {
                "id": "m1",
                "author": {"role": "assistant"},
                "create_time": 1717200010.0,
                "content": {"content_type": "text", "parts": ["I can help with that."]},
                "metadata": {}
              }
            },
            "m2": {
              "id": "m2", "parent": "m1", "children": [],
              "message": {
                "id": "m2",
                "author": {"role": "assistant"},
                "create_time": 1717200020.0,
                "content": {"content_type": "text", "parts": ["Here is more detail."]},
                "metadata": {"model_slug": "gpt-4o"}
              }
            }
          }
        }]"#;

        let mut seen = HashSet::new();
        let conv: ConvRaw = serde_json::from_str::<Vec<ConvRaw>>(json).unwrap().remove(0);
        let rows = linearize_conversation(&conv, &mut seen);
        assert_eq!(rows.len(), 2);

        // Both rows should have model populated (m1 via fallback, m2 directly).
        let m1 = rows.iter().find(|r| r.message_id == "m1").expect("m1");
        let m2 = rows.iter().find(|r| r.message_id == "m2").expect("m2");
        assert_eq!(
            m1.model.as_deref(),
            Some("gpt-4o"),
            "m1 should inherit model from conversation-level scan"
        );
        assert_eq!(m2.model.as_deref(), Some("gpt-4o"), "m2 has it directly");
    }
}
