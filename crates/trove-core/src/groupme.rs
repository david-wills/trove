//! GroupMe — incremental pull via the official v3 API using a personal access
//! token (no OAuth, no app registration); token pasted from dev.groupme.com.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/groupme.md.
//!
//! Two conversation kinds, one unified correspondence sink:
//!
//! - **Groups** — `GET /v3/groups` lists every group; each group is drained
//!   newest-first via `GET /v3/groups/{id}/messages?before_id=…` until an
//!   already-seen message id is hit (the watermark). Re-running is safe; the
//!   watermark per group is the highest message id stored.
//! - **DMs** — `GET /v3/chats` lists DM conversations; each is drained via
//!   `GET /v3/direct_messages?other_user_id=…&before_id=…` until already-seen.
//!
//! Messages land in:
//!   - **raw** `correspondence/groupme/raw/YYYY-MM.jsonl` — full API object,
//!     partitioned by the message's local month.
//!   - **contract** `correspondence/groupme/YYYY-MM.jsonl` — one [`Message`]
//!     per API message, deduped by GroupMe message id (`guid`).
//!
//! ToS note: GroupMe's caching clause applies to a 3-day window / ~100
//! messages per day. Personal archiving enforcement is effectively nil, and the
//! official ZIP export (Settings → Export Data) is a clean alternative for
//! bulk history. A future Import arm for that ZIP can share the same guid
//! namespace for seamless dedupe.
//!
//! Watermarks live in `.trove/groupme-sync.json` (rebuildable, non-secret).
//! The token lives in `.trove/sync/groupme.json` (0600 secret store).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::correspondence::{AttachmentMeta, Message};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const API_BASE: &str = "https://api.groupme.com/v3";
/// GroupMe allows up to 100 messages per page.
const PAGE_SIZE: u64 = 100;
/// Polite inter-request delay — GroupMe's undocumented rate limit is generous;
/// 200 ms keeps us well within it.
const REQ_DELAY: Duration = Duration::from_millis(200);
/// Short enough not to stall the watcher loop on a dead connection.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Raw firehose directory.
const RAW_DIR: &str = "correspondence/groupme/raw";
/// Contract directory (same source name as the raw parent's sibling).
const CONTRACT_DIR: &str = "correspondence/groupme";
/// Watermark file — non-secret, rebuildable.
const SYNC_FILE: &str = ".trove/groupme-sync.json";
/// Secret-store service id.
const SERVICE: &str = "groupme";
/// 30-minute cadence — group chats are bursty; a 30-min window is a
/// reasonable latency/API-cost balance.
pub const GROUPME_SYNC_SECS: u64 = 1800;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!("groupme synced — {} group, {} dm messages", c("group_messages"), c("dm_messages"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("groupme sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    let total = c("group_messages") + c("dm_messages");
    let headline = if total == 0 {
        "GroupMe is up to date — no new messages".to_string()
    } else {
        format!(
            "GroupMe synced — {} group messages, {} DMs",
            c("group_messages"),
            c("dm_messages"),
        )
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "groupme",
        name: "GroupMe",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your GroupMe group and direct-message history from the \
                      official v3 API using a personal access token. First sync \
                      backfills; later syncs are incremental per conversation.",
        domain: "correspondence",
        vault_path: "correspondence/groupme/",
        toggleable: true,
        setup: &[
            "Sign in at dev.groupme.com → Access Token, paste your token in the connect card.",
            "First sync backfills all group and DM history; later syncs fetch only new messages.",
        ],
        caveats: "GroupMe's terms of service have a caching clause for API consumers. \
                  Personal archiving enforcement is effectively nil, but if that is a concern, \
                  use the official ZIP export (Settings → Export Data) instead — a future \
                  Import arm will parse that ZIP and deduplicate against API rows on the \
                  same message id.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(GROUPME_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("groupme"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — personal access token from dev.groupme.com).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your GroupMe access token from dev.groupme.com → Access Token");
    }
    // Verify the token is valid with a cheap /users/me call.
    let client = GroupMeClient::new(API_BASE.to_string(), token.to_string());
    match client.get("/users/me") {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "GroupMe rejected the token (401) — check it at dev.groupme.com → Access Token"
        ),
        Err(e) => bail!("GroupMe /users/me check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &crate::sync::oauth::TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        // The display label is the token's first 8 chars elided — we don't
        // store the username separately, and GroupMe access tokens are opaque.
        let label = format!("{}…", &token.access_token[..token.access_token.len().min(8)]);
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "groupme",
    display_name: "GroupMe",
    methods: &[ConnectMethod::TokenPaste {
        label: "Access token",
        help: "Sign in to dev.groupme.com → click your avatar → Access Token. \
               Paste the token here — it never leaves your device.",
        placeholder: "your GroupMe access token",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["groupme"],
    setup: &[
        "Visit dev.groupme.com, click your avatar in the top-right, and copy your Access Token.",
        "Paste it here. All sync runs locally; the token is stored in your encrypted vault.",
    ],
};

// ---------------------------------------------------------------------------
// Watermark cursor — per group/dm id, the highest (newest) message id seen.
// GroupMe message ids are numeric strings that sort lexicographically when
// zero-padded, but since we compare them as strings and stop on first seen,
// we just track the newest id we have stored.

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    /// group_id → newest message id stored.
    #[serde(default)]
    group_watermarks: HashMap<String, String>,
    /// other_user_id → newest DM message id stored.
    #[serde(default)]
    dm_watermarks: HashMap<String, String>,
}

fn load_cursor(vault: &Vault) -> Result<Cursor> {
    let path = vault.root().join(SYNC_FILE);
    if !path.exists() {
        return Ok(Cursor::default());
    }
    let body = std::fs::read_to_string(&path).context("reading groupme-sync.json")?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn save_cursor(vault: &Vault, cursor: &Cursor) -> Result<()> {
    let path = vault.root().join(SYNC_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating .trove/")?;
    }
    crate::store::write_json_atomic(&path, cursor)
}

// ---------------------------------------------------------------------------
// Pull outcome.

struct PullStats {
    counts: BTreeMap<&'static str, u64>,
}

// ---------------------------------------------------------------------------
// HTTP layer.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Thin injectable HTTP client. Tests use a stub; production hits the real API.
trait GroupMeApi {
    /// `GET <path>` and return the parsed JSON body.
    fn get(&self, path: &str) -> Result<Value, FetchError>;
    /// `GET <path>?<query_pairs>` and return the parsed JSON body.
    fn get_q(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, FetchError>;
}

struct GroupMeClient {
    base: String,
    token: String,
}

impl GroupMeClient {
    fn new(base: String, token: String) -> Self {
        GroupMeClient { base, token }
    }
}

impl GroupMeApi for GroupMeClient {
    fn get(&self, path: &str) -> Result<Value, FetchError> {
        self.get_q(path, &[])
    }

    fn get_q(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, FetchError> {
        let url = format!("{}{}", self.base, path);
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .query("token", &self.token);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parse error: {e}")))?;
                Ok(v)
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(304, _)) => {
                // "No messages" is 304 for some GroupMe endpoints.
                Ok(Value::Null)
            }
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Main pull logic (injectable for tests).

fn pull(vault: &Vault) -> Result<PullStats> {
    let token = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| anyhow::anyhow!("GroupMe not connected — paste your access token"))?;
    let client = GroupMeClient::new(API_BASE.to_string(), token.access_token);
    pull_with(&client, vault)
}

fn pull_with(api: &dyn GroupMeApi, vault: &Vault) -> Result<PullStats> {
    let mut cursor = load_cursor(vault)?;
    let mut seen = vault.correspondence_guids("groupme")?;
    let mut messages: Vec<Message> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut group_count: u64 = 0;
    let mut dm_count: u64 = 0;

    // Fetch the vault owner's GroupMe user id so we can set from_me correctly.
    // GET /users/me → response.id (string or number).
    let my_id: String = match api.get("/users/me") {
        Ok(v) => {
            v["response"]["id"]
                .as_str()
                .map(str::to_string)
                .or_else(|| v["response"]["id"].as_u64().map(|n| n.to_string()))
                .unwrap_or_default()
        }
        Err(_) => String::new(), // tolerate transient failure; from_me stays false
    };

    // 1. Groups.
    let groups = fetch_groups(api)?;
    for group in &groups {
        let group_id = group["id"].as_str().unwrap_or_default();
        let group_name = group["name"].as_str().unwrap_or_default();
        if group_id.is_empty() {
            continue;
        }
        let watermark = cursor.group_watermarks.get(group_id).cloned();
        let (msgs, raws, new_watermark) = drain_group(api, group_id, group_name, watermark, &mut seen, &my_id)?;
        group_count += msgs.len() as u64;
        messages.extend(msgs);
        raw_rows.extend(raws);
        if let Some(wm) = new_watermark {
            cursor.group_watermarks.insert(group_id.to_string(), wm);
        }
    }

    // 2. Direct messages.
    let chats = fetch_chats(api)?;
    for chat in &chats {
        let other_user_id = chat["other_user"]["id"].as_u64()
            .map(|id| id.to_string())
            .or_else(|| chat["other_user"]["id"].as_str().map(str::to_string))
            .unwrap_or_default();
        let other_name = chat["other_user"]["name"].as_str().unwrap_or_default();
        if other_user_id.is_empty() {
            continue;
        }
        let watermark = cursor.dm_watermarks.get(&other_user_id).cloned();
        let (msgs, raws, new_watermark) = drain_dms(api, &other_user_id, other_name, watermark, &mut seen, &my_id)?;
        dm_count += msgs.len() as u64;
        messages.extend(msgs);
        raw_rows.extend(raws);
        if let Some(wm) = new_watermark {
            cursor.dm_watermarks.insert(other_user_id, wm);
        }
    }

    // Write contract rows.
    if !messages.is_empty() {
        vault.append_messages(&messages)?;
    }

    // Write raw rows.
    if !raw_rows.is_empty() {
        vault
            .stream(RAW_DIR, Partition::Month)
            .append(&raw_rows, |v| v["_ts"].as_str().unwrap_or_default())?;
    }

    save_cursor(vault, &cursor)?;

    let mut counts = BTreeMap::new();
    counts.insert("group_messages", group_count);
    counts.insert("dm_messages", dm_count);
    Ok(PullStats { counts })
}

// ---------------------------------------------------------------------------
// Group list.

fn fetch_groups(api: &dyn GroupMeApi) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page = 1u32;
    loop {
        let resp = api
            .get_q(
                "/groups",
                &[("per_page", "100"), ("page", &page.to_string()), ("omit", "memberships")],
            )
            .map_err(|e| anyhow::anyhow!("fetch groups page {page}: {e}"))?;
        let arr = match resp["response"].as_array() {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };
        let len = arr.len();
        out.extend(arr);
        if len < 100 { break; }
        page += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Chat (DM conversation) list.

fn fetch_chats(api: &dyn GroupMeApi) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page = 1u32;
    loop {
        let resp = api
            .get_q("/chats", &[("per_page", "100"), ("page", &page.to_string())])
            .map_err(|e| anyhow::anyhow!("fetch chats page {page}: {e}"))?;
        let arr = match resp["response"].as_array() {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };
        let len = arr.len();
        out.extend(arr);
        if len < 100 { break; }
        page += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Drain a group's message history using before_id pagination.
// Returns (contract rows, raw rows, new watermark).

fn drain_group(
    api: &dyn GroupMeApi,
    group_id: &str,
    group_name: &str,
    watermark: Option<String>,
    seen: &mut HashSet<String>,
    my_id: &str,
) -> Result<(Vec<Message>, Vec<Value>, Option<String>)> {
    let mut messages = Vec::new();
    let mut raw_rows = Vec::new();
    let mut new_watermark: Option<String> = None;
    let mut before_id: Option<String> = None;
    let path = format!("/groups/{group_id}/messages");

    'pages: loop {
        let limit_str = PAGE_SIZE.to_string();
        let mut params: Vec<(&str, &str)> = vec![("limit", &limit_str)];
        // before_id must be stable across the closure's lifetime
        let bid;
        if let Some(ref b) = before_id {
            bid = b.clone();
            params.push(("before_id", &bid));
        }

        std::thread::sleep(REQ_DELAY);
        let resp = api
            .get_q(&path, &params)
            .map_err(|e| anyhow::anyhow!("group {group_id} messages: {e}"))?;

        // 304 / null response = no messages.
        if resp.is_null() {
            break;
        }

        let msgs = match resp["response"]["messages"].as_array() {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };

        for raw in &msgs {
            let id = raw["id"].as_str().unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            // If we've already stored this id, we've reached the watermark.
            if watermark.as_deref() == Some(id) || seen.contains(id) {
                break 'pages;
            }
            if !seen.insert(id.to_string()) {
                continue;
            }
            // Track the newest id (first page, first message is newest).
            if new_watermark.is_none() {
                new_watermark = Some(id.to_string());
            }
            let ts = unix_to_local(raw["created_at"].as_i64().unwrap_or(0));
            let mut m = Message::new("groupme", ts.clone());
            m.guid = id.to_string();
            m.chat = group_id.to_string();
            m.chat_name = group_name.to_string();
            let user_id = raw["user_id"].as_str().unwrap_or_default();
            m.from_me = !my_id.is_empty() && user_id == my_id;
            if !m.from_me {
                m.sender = user_id.to_string();
                m.sender_name = raw["name"].as_str().unwrap_or_default().to_string();
            }
            m.text = raw["text"].as_str().unwrap_or_default().to_string();
            m.attachments = parse_attachments(raw);
            if raw["system"].as_bool().unwrap_or(false) {
                m.kind = "event".into();
            }

            // Raw row: full fidelity + synthetic _ts for partition routing.
            let mut row = raw.clone();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("_ts".into(), Value::String(ts.clone()));
                obj.insert("_kind".into(), Value::String("group".into()));
            }
            raw_rows.push(row);
            messages.push(m);
        }

        // Advance before_id to the last (oldest) message in this page.
        let oldest_id = msgs.last()
            .and_then(|m| m["id"].as_str())
            .map(str::to_string);
        match oldest_id {
            Some(id) if msgs.len() as u64 == PAGE_SIZE => before_id = Some(id),
            _ => break,
        }
    }

    Ok((messages, raw_rows, new_watermark))
}

// ---------------------------------------------------------------------------
// Drain a DM conversation using before_id + other_user_id pagination.

fn drain_dms(
    api: &dyn GroupMeApi,
    other_user_id: &str,
    other_name: &str,
    watermark: Option<String>,
    seen: &mut HashSet<String>,
    my_id: &str,
) -> Result<(Vec<Message>, Vec<Value>, Option<String>)> {
    let mut messages = Vec::new();
    let mut raw_rows = Vec::new();
    let mut new_watermark: Option<String> = None;
    let mut before_id: Option<String> = None;

    'pages: loop {
        let limit_str = PAGE_SIZE.to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("other_user_id", other_user_id),
            ("limit", &limit_str),
        ];
        let bid;
        if let Some(ref b) = before_id {
            bid = b.clone();
            params.push(("before_id", &bid));
        }

        std::thread::sleep(REQ_DELAY);
        let resp = api
            .get_q("/direct_messages", &params)
            .map_err(|e| anyhow::anyhow!("DM {other_user_id}: {e}"))?;

        if resp.is_null() {
            break;
        }

        let msgs = match resp["response"]["direct_messages"].as_array() {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };

        for raw in &msgs {
            let id = raw["id"].as_str().unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            if watermark.as_deref() == Some(id) || seen.contains(id) {
                break 'pages;
            }
            if !seen.insert(id.to_string()) {
                continue;
            }
            if new_watermark.is_none() {
                new_watermark = Some(id.to_string());
            }
            let ts = unix_to_local(raw["created_at"].as_i64().unwrap_or(0));
            let mut m = Message::new("groupme", ts.clone());
            m.guid = id.to_string();
            // conversation_id is sender_id + "+" + recipient_id
            m.chat = raw["conversation_id"].as_str().unwrap_or_default().to_string();
            m.chat_name = other_name.to_string();
            let sender_id = raw["sender_id"].as_str().unwrap_or_default();
            m.from_me = !my_id.is_empty() && sender_id == my_id;
            if !m.from_me {
                m.sender = sender_id.to_string();
                m.sender_name = raw["name"].as_str().unwrap_or_default().to_string();
            }
            m.text = raw["text"].as_str().unwrap_or_default().to_string();
            m.attachments = parse_attachments(raw);

            let mut row = raw.clone();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("_ts".into(), Value::String(ts.clone()));
                obj.insert("_kind".into(), Value::String("dm".into()));
            }
            raw_rows.push(row);
            messages.push(m);
        }

        let oldest_id = msgs.last()
            .and_then(|m| m["id"].as_str())
            .map(str::to_string);
        match oldest_id {
            Some(id) if msgs.len() as u64 == PAGE_SIZE => before_id = Some(id),
            _ => break,
        }
    }

    Ok((messages, raw_rows, new_watermark))
}

// ---------------------------------------------------------------------------
// Helpers.

/// GroupMe `created_at` is a Unix timestamp (seconds). Convert to local RFC3339.
fn unix_to_local(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_default()
}

/// Attachment array → [`AttachmentMeta`] vec. GroupMe attachment types:
/// `image` (`url`), `video` (`url`), `file` (`file_id`),
/// `location` (`name`, `lat`, `lng`),
/// `reply` and `mentions` — pure message pointers, not surfaced as file entries.
/// `split` and `emoji` — metadata-only signals, not surfaced as file entries.
fn parse_attachments(msg: &Value) -> Vec<AttachmentMeta> {
    let arr = match msg["attachments"].as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|att| {
            let kind = att["type"].as_str().unwrap_or("");
            match kind {
                "image" => {
                    let url = att["url"].as_str().unwrap_or("");
                    let name = url.rsplit('/').next().unwrap_or("image");
                    Some(AttachmentMeta {
                        name: if name.is_empty() { "image".into() } else { name.to_string() },
                        mime: "image/*".into(),
                        bytes: 0,
                    })
                }
                "video" => {
                    let url = att["url"].as_str().unwrap_or("");
                    let name = url.rsplit('/').next().unwrap_or("video");
                    Some(AttachmentMeta {
                        name: if name.is_empty() { "video".into() } else { name.to_string() },
                        mime: "video/*".into(),
                        bytes: 0,
                    })
                }
                "file" => {
                    // GroupMe file attachments carry a file_id; no direct URL.
                    let file_id = att["file_id"].as_str().unwrap_or("unknown");
                    Some(AttachmentMeta {
                        name: format!("file:{file_id}"),
                        mime: String::new(),
                        bytes: 0,
                    })
                }
                "location" => Some(AttachmentMeta {
                    name: format!(
                        "location:{}",
                        att["name"].as_str().unwrap_or("unknown")
                    ),
                    mime: String::new(),
                    bytes: 0,
                }),
                _ => None, // reply/mentions/emoji/split — pointers or metadata-only, no file entry
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-groupme-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Stub API.

    struct StubApi {
        /// path+query → response Value
        responses: HashMap<String, Value>,
    }

    impl StubApi {
        fn new() -> Self {
            StubApi { responses: HashMap::new() }
        }

        fn set(&mut self, key: &str, val: Value) {
            self.responses.insert(key.to_string(), val);
        }
    }

    impl GroupMeApi for StubApi {
        fn get(&self, path: &str) -> Result<Value, FetchError> {
            self.get_q(path, &[])
        }

        fn get_q(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, FetchError> {
            // Build a sortable key from path + sorted params, excluding size
            // hints (`limit`, `per_page`) so tests don't have to encode them.
            let mut ps: Vec<String> = params.iter()
                .filter(|(k, _)| *k != "limit" && *k != "per_page")
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            ps.sort();
            let key = if ps.is_empty() { path.to_string() } else { format!("{path}?{}", ps.join("&")) };
            self.responses
                .get(&key)
                .cloned()
                .ok_or_else(|| FetchError::Other(format!("stub: no response for {key}")))
        }
    }

    // ---------------------------------------------------------------------------
    // Test helpers.

    /// A minimal group message fixture (API shape).
    fn group_msg(id: &str, user_id: &str, name: &str, text: &str, ts: i64) -> Value {
        json!({
            "id": id,
            "source_guid": format!("sg-{id}"),
            "created_at": ts,
            "user_id": user_id,
            "group_id": "grp1",
            "name": name,
            "avatar_url": "https://i.groupme.com/avatar.jpeg",
            "text": text,
            "system": false,
            "favorited_by": ["u999"],
            "attachments": []
        })
    }

    /// A DM message fixture.
    fn dm_msg(id: &str, sender_id: &str, name: &str, text: &str, ts: i64) -> Value {
        json!({
            "id": id,
            "source_guid": format!("sg-{id}"),
            "conversation_id": format!("{sender_id}+9999"),
            "created_at": ts,
            "user_id": sender_id,
            "sender_id": sender_id,
            "sender_type": "user",
            "recipient_id": "9999",
            "name": name,
            "avatar_url": "https://i.groupme.com/avatar.jpeg",
            "text": text,
            "favorited_by": [],
            "attachments": []
        })
    }

    // ---------------------------------------------------------------------------

    #[test]
    fn unix_to_local_is_sane() {
        let ts = unix_to_local(1_749_600_000); // 2025-06 range
        assert!(ts.starts_with("2025-"), "ts={ts}");
        assert!(ts.contains('T'), "should be RFC3339: {ts}");
    }

    #[test]
    fn parse_attachments_image() {
        let msg = json!({
            "attachments": [
                {"type": "image", "url": "https://i.groupme.com/750x750.jpeg/cat.jpg"}
            ]
        });
        let atts = parse_attachments(&msg);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "cat.jpg");
        assert_eq!(atts[0].mime, "image/*");
    }

    #[test]
    fn parse_attachments_location() {
        let msg = json!({
            "attachments": [
                {"type": "location", "name": "Eiffel Tower", "lat": "48.8584", "lng": "2.2945"}
            ]
        });
        let atts = parse_attachments(&msg);
        assert_eq!(atts.len(), 1);
        assert!(atts[0].name.contains("Eiffel Tower"), "name: {}", atts[0].name);
    }

    #[test]
    fn parse_attachments_emoji_skipped() {
        let msg = json!({
            "attachments": [
                {"type": "emoji", "placeholder": "😀", "charmap": [[1, 0]]}
            ]
        });
        let atts = parse_attachments(&msg);
        assert!(atts.is_empty(), "emoji attachment should not produce a file entry");
    }

    /// Add the /users/me stub that pull_with always calls first.
    fn set_me(api: &mut StubApi, user_id: &str) {
        api.set(
            "/users/me",
            json!({"response": {"id": user_id, "name": "Vault Owner"}}),
        );
    }

    #[test]
    fn group_messages_persisted_with_contract_and_raw() {
        let vault = temp_vault("group");
        let mut api = StubApi::new();
        set_me(&mut api, "u999"); // owner is u999 — not a sender in this test

        // /groups returns one group.
        api.set(
            "/groups?omit=memberships&page=1",
            json!({"response": [{"id": "grp1", "name": "Adventure Club"}]}),
        );
        // /groups/grp1/messages — one page of 2 messages, newest first.
        let ts1 = 1_750_000_100i64;
        let ts2 = 1_750_000_000i64;
        api.set(
            "/groups/grp1/messages",
            json!({"response": {"messages": [
                group_msg("msg2", "u1", "Alice", "hello", ts1),
                group_msg("msg1", "u2", "Bob", "world", ts2)
            ]}}),
        );
        // /chats returns empty (no DMs).
        api.set("/chats?page=1", json!({"response": []}));

        let stats = pull_with(&api, &vault).unwrap();
        assert_eq!(stats.counts["group_messages"], 2);
        assert_eq!(stats.counts["dm_messages"], 0);

        // Contract rows written.
        let month = unix_to_local(ts1)[..7].to_string();
        let path = vault.root().join(format!("correspondence/groupme/{month}.jsonl"));
        assert!(path.exists(), "contract file should exist");
        let body = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<Message> = body
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].guid, "msg2");
        assert_eq!(rows[0].chat, "grp1");
        assert_eq!(rows[0].chat_name, "Adventure Club");
        assert_eq!(rows[0].sender_name, "Alice");
        assert_eq!(rows[0].text, "hello");
        assert_eq!(rows[0].source, "groupme");
        assert!(!rows[0].from_me, "u1 != owner u999");

        // Raw rows written.
        let raw_path = vault.root().join(format!("correspondence/groupme/raw/{month}.jsonl"));
        assert!(raw_path.exists(), "raw file should exist");
        let raw_body = std::fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw_body.lines().count(), 2, "2 raw rows");
        let raw0: Value = serde_json::from_str(raw_body.lines().next().unwrap()).unwrap();
        assert_eq!(raw0["_kind"].as_str(), Some("group"));
        assert!(raw0["_ts"].as_str().is_some(), "synthetic _ts present");
        assert!(raw0["favorited_by"].is_array(), "full fidelity: favorited_by present");

        // Watermark saved.
        let cursor = load_cursor(&vault).unwrap();
        assert_eq!(cursor.group_watermarks.get("grp1").map(String::as_str), Some("msg2"));
    }

    #[test]
    fn dm_messages_persisted() {
        let vault = temp_vault("dm");
        let mut api = StubApi::new();
        set_me(&mut api, "99"); // owner is 99 — not sender 77

        api.set("/groups?omit=memberships&page=1", json!({"response": []}));
        api.set(
            "/chats?page=1",
            json!({"response": [{"other_user": {"id": 77, "name": "Carol"}}]}),
        );
        let ts = 1_750_001_000i64;
        api.set(
            "/direct_messages?other_user_id=77",
            json!({"response": {"direct_messages": [
                dm_msg("dm1", "77", "Carol", "hey there", ts)
            ]}}),
        );

        let stats = pull_with(&api, &vault).unwrap();
        assert_eq!(stats.counts["dm_messages"], 1);

        let month = unix_to_local(ts)[..7].to_string();
        let path = vault.root().join(format!("correspondence/groupme/{month}.jsonl"));
        let body = std::fs::read_to_string(&path).unwrap();
        let row: Message = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(row.guid, "dm1");
        assert_eq!(row.chat_name, "Carol");
        assert_eq!(row.sender, "77");
        assert_eq!(row.text, "hey there");
        assert!(!row.from_me, "sender 77 != owner 99");

        let cursor = load_cursor(&vault).unwrap();
        assert_eq!(cursor.dm_watermarks.get("77").map(String::as_str), Some("dm1"));
    }

    #[test]
    fn watermark_stops_re_fetch() {
        let vault = temp_vault("wm");
        let mut api = StubApi::new();
        set_me(&mut api, "u999");
        let ts1 = 1_750_000_100i64;
        let ts2 = 1_750_000_000i64;

        api.set(
            "/groups?omit=memberships&page=1",
            json!({"response": [{"id": "grp1", "name": "Test"}]}),
        );
        api.set(
            "/groups/grp1/messages",
            json!({"response": {"messages": [
                group_msg("msg2", "u1", "Alice", "second", ts1),
                group_msg("msg1", "u2", "Bob", "first", ts2)
            ]}}),
        );
        api.set("/chats?page=1", json!({"response": []}));

        // First pull — gets both.
        let s1 = pull_with(&api, &vault).unwrap();
        assert_eq!(s1.counts["group_messages"], 2);

        // Second pull — watermark = "msg2"; both ids already in seen; nothing new.
        let s2 = pull_with(&api, &vault).unwrap();
        assert_eq!(s2.counts["group_messages"], 0, "watermark stops re-fetch");
    }

    #[test]
    fn reimport_is_noop() {
        let vault = temp_vault("noop");
        let mut api = StubApi::new();
        set_me(&mut api, "u999");
        let ts = 1_750_002_000i64;

        api.set(
            "/groups?omit=memberships&page=1",
            json!({"response": [{"id": "g1", "name": "G1"}]}),
        );
        api.set(
            "/groups/g1/messages",
            json!({"response": {"messages": [group_msg("m1", "u1", "X", "hi", ts)]}}),
        );
        api.set("/chats?page=1", json!({"response": []}));

        pull_with(&api, &vault).unwrap();
        let before = std::fs::read_to_string(
            vault.root().join(format!("correspondence/groupme/{}.jsonl", &unix_to_local(ts)[..7]))
        ).unwrap();

        pull_with(&api, &vault).unwrap();
        let after = std::fs::read_to_string(
            vault.root().join(format!("correspondence/groupme/{}.jsonl", &unix_to_local(ts)[..7]))
        ).unwrap();
        assert_eq!(before, after, "file unchanged on re-run");
    }

    #[test]
    fn message_with_image_attachment_metadata_only() {
        let vault = temp_vault("attach");
        let mut api = StubApi::new();
        set_me(&mut api, "u999");
        let ts = 1_750_003_000i64;
        let msg = json!({
            "id": "m_img",
            "source_guid": "sg-m_img",
            "created_at": ts,
            "user_id": "u1",
            "group_id": "g1",
            "name": "Alice",
            "text": "check this out",
            "system": false,
            "favorited_by": [],
            "attachments": [{"type": "image", "url": "https://i.groupme.com/750x750.jpeg/photo.jpg"}]
        });
        api.set(
            "/groups?omit=memberships&page=1",
            json!({"response": [{"id": "g1", "name": "G1"}]}),
        );
        api.set("/groups/g1/messages", json!({"response": {"messages": [msg]}}));
        api.set("/chats?page=1", json!({"response": []}));

        pull_with(&api, &vault).unwrap();
        let month = &unix_to_local(ts)[..7];
        let body = std::fs::read_to_string(
            vault.root().join(format!("correspondence/groupme/{month}.jsonl"))
        ).unwrap();
        let row: Message = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(row.attachments.len(), 1);
        assert_eq!(row.attachments[0].name, "photo.jpg");
        assert_eq!(row.attachments[0].bytes, 0, "metadata only, not fetched");
    }

    #[test]
    fn from_me_set_correctly_for_group_and_dm() {
        let vault = temp_vault("from_me");
        let mut api = StubApi::new();
        // Owner is "u1".
        set_me(&mut api, "u1");
        let ts1 = 1_750_005_100i64;
        let ts2 = 1_750_005_000i64;

        api.set(
            "/groups?omit=memberships&page=1",
            json!({"response": [{"id": "grp1", "name": "Test Group"}]}),
        );
        // Two group messages: one from the owner (u1), one from u2.
        api.set(
            "/groups/grp1/messages",
            json!({"response": {"messages": [
                group_msg("gm2", "u2", "Bob", "hey", ts1),
                group_msg("gm1", "u1", "Me", "hello", ts2)
            ]}}),
        );

        // One DM conversation: owner (u1) sent one, the other user (u2) sent one.
        api.set(
            "/chats?page=1",
            json!({"response": [{"other_user": {"id": 2, "name": "Bob"}}]}),
        );
        let ts3 = 1_750_006_100i64;
        let ts4 = 1_750_006_000i64;
        api.set(
            "/direct_messages?other_user_id=2",
            json!({"response": {"direct_messages": [
                dm_msg("dm2", "u1", "Me", "I replied", ts3),
                dm_msg("dm1", "2", "Bob", "hi", ts4)
            ]}}),
        );

        pull_with(&api, &vault).unwrap();

        let month = unix_to_local(ts1)[..7].to_string();
        let path = vault.root().join(format!("correspondence/groupme/{month}.jsonl"));
        let body = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<Message> = body
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        // Group: gm2 (Bob) is not from owner; gm1 (u1) is from owner.
        let gm2 = rows.iter().find(|m| m.guid == "gm2").expect("gm2");
        let gm1 = rows.iter().find(|m| m.guid == "gm1").expect("gm1");
        assert!(!gm2.from_me, "gm2 is from Bob, not owner");
        assert!(!gm2.sender.is_empty(), "non-owner has sender set");
        assert!(gm1.from_me, "gm1 is from owner u1");
        assert!(gm1.sender.is_empty(), "owner message: sender is blank");

        // DM: dm2 is from owner (u1); dm1 is from other user.
        let dm2 = rows.iter().find(|m| m.guid == "dm2").expect("dm2");
        let dm1 = rows.iter().find(|m| m.guid == "dm1").expect("dm1");
        assert!(dm2.from_me, "dm2 sent by owner u1");
        assert!(dm2.sender.is_empty(), "owner DM: sender is blank");
        assert!(!dm1.from_me, "dm1 sent by Bob");
        assert!(!dm1.sender.is_empty(), "non-owner DM: sender is set");
    }

    #[test]
    fn parse_attachments_video_and_file() {
        let msg = json!({
            "attachments": [
                {"type": "video", "url": "https://v.groupme.com/clip.mp4"},
                {"type": "file", "file_id": "abc-123"}
            ]
        });
        let atts = parse_attachments(&msg);
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].name, "clip.mp4");
        assert_eq!(atts[0].mime, "video/*");
        assert!(atts[1].name.contains("abc-123"), "file id in name: {}", atts[1].name);
    }

    #[test]
    fn from_me_graceful_when_users_me_fails() {
        // If /users/me is not in the stub (simulates transient failure),
        // pull_with should succeed with from_me=false for all messages.
        let vault = temp_vault("no_me");
        let mut api = StubApi::new();
        // Intentionally do NOT set /users/me — simulates failure.
        let ts = 1_750_007_000i64;
        api.set(
            "/groups?omit=memberships&page=1",
            json!({"response": [{"id": "g1", "name": "G1"}]}),
        );
        api.set(
            "/groups/g1/messages",
            json!({"response": {"messages": [group_msg("m1", "u1", "Alice", "hi", ts)]}}),
        );
        api.set("/chats?page=1", json!({"response": []}));

        // Should not error — from_me defaults to false when owner id is unknown.
        let stats = pull_with(&api, &vault).unwrap();
        assert_eq!(stats.counts["group_messages"], 1);
        let month = &unix_to_local(ts)[..7];
        let body = std::fs::read_to_string(
            vault.root().join(format!("correspondence/groupme/{month}.jsonl"))
        ).unwrap();
        let row: Message = serde_json::from_str(body.trim()).unwrap();
        assert!(!row.from_me, "from_me defaults to false when /users/me fails");
    }

    #[test]
    fn old_message_line_back_compat() {
        // A pre-groupme message line with no groupme-specific fields deserializes cleanly.
        let line = r#"{"ts":"2026-06-10T09:00:00-07:00","source":"groupme","chat":"grp1","from_me":false,"kind":"message","text":"hi"}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hi");
        assert_eq!(m.chat, "grp1");
    }
}
