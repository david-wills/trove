//! Microsoft Teams chat collector via Microsoft Graph API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/microsoft-teams.md.
//!
//! Pulls chat messages from every connected Microsoft account into the unified
//! correspondence stream (`correspondence/microsoft-teams/YYYY-MM.jsonl`).
//! Reuses the shared "microsoft" connection from [`crate::outlook`] and the
//! per-account token store ([`crate::outlook::microsoft_fresh_token`]).
//!
//! **Scope dependency.** Teams chat requires `Chat.Read` (or `Chat.ReadBasic`)
//! on the Microsoft provider. The provider currently bundles
//! `Mail.Read Calendars.Read User.Read offline_access`; `Chat.Read` must be
//! added to the scope list in [`crate::outlook::MICROSOFT`] (one-line edit,
//! the Entra app must also grant the new permission) before this integration
//! can fetch real data. Flagged as Needs-David(scope-update).
//!
//! **Strategy.** For each account, the collector lists every chat via
//! `GET /me/chats` (paginated), then for each chat reads new messages after
//! the per-chat high-watermark cursor (`createdDateTime` of the newest stored
//! message). On the first run for a chat the full history is fetched.
//!
//! **Raw layer.** Full-fidelity Graph chatMessage JSON is stored at
//! `correspondence/microsoft-teams/raw/YYYY-MM.jsonl` unconditionally.
//!
//! **Contract layer.** Messages are converted to the correspondence
//! [`crate::correspondence::Message`] shape and stored via
//! [`Vault::append_messages`] (source = "microsoft-teams", guid = Graph
//! message id).
//!
//! **Meeting transcripts.** Transcript collection is parked behind the
//! unratified meetings contract and a Needs-David flag; the raw sidecar path
//! (`meetings/microsoft-teams/raw/`) is wired up here but the parser is not
//! shipped. That slice ships after the meetings contract ratifies.
//!
//! **Cursor.** `.trove/microsoft-teams-sync.json` (non-secret, rebuildable)
//! holds per-account, per-chat watermarks (the newest stored message's
//! `createdDateTime`). The cursor advances per-chat ONLY after its batch is
//! written (drain-don't-strand).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::correspondence::{AttachmentMeta, Message};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

const SOURCE: &str = "microsoft-teams";
const SYNC_FILE: &str = ".trove/microsoft-teams-sync.json";
const RAW_DIR: &str = "correspondence/microsoft-teams/raw";
const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between Teams sync passes — same cadence as Outlook email.
pub const TEAMS_SYNC_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// Graph API response shapes.

/// One chat entry from `GET /me/chats`.
#[derive(Debug, Deserialize)]
pub(crate) struct TeamsChatInfo {
    pub id: String,
    #[serde(default)]
    pub topic: Option<String>,
}

// ---------------------------------------------------------------------------
// Cursor state (non-secret, rebuildable).

/// Watermark for one chat in one account: the newest stored message's
/// `createdDateTime` (RFC3339). Stored as a string so it can round-trip
/// through Graph's filter parameter verbatim.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ChatCursor {
    /// Display label (the topic or chat id) — for the index, not logic.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    label: String,
    /// Newest stored message's `createdDateTime`; `None` means first run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    /// Total messages ever written for this chat.
    #[serde(default)]
    messages: u64,
}

/// Per-account sync state.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct AccountState {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    email: String,
    /// Per-chat watermarks, keyed by chat id.
    #[serde(default)]
    chats: BTreeMap<String, ChatCursor>,
    /// Total messages across all chats for this account.
    #[serde(default)]
    messages: u64,
    /// Last per-account error string for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Top-level sync file.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    updated: String,
    #[serde(default)]
    accounts: BTreeMap<String, AccountState>,
}

impl Vault {
    fn read_teams_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_teams_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Graph HTTP client — base URL injected for testability.

/// Error variants that drive the per-account error handling.
#[derive(Debug)]
enum TeamsError {
    RateLimited,
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for TeamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TeamsError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            TeamsError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            TeamsError::Other(m) => write!(f, "{m}"),
        }
    }
}

fn classify(code: u16, resp: ureq::Response, ctx: &str) -> TeamsError {
    match code {
        401 => TeamsError::Unauthorized,
        429 => TeamsError::RateLimited,
        _ => {
            let body = resp.into_string().unwrap_or_default();
            TeamsError::Other(format!(
                "HTTP {code}{ctx}: {}",
                body.chars().take(300).collect::<String>()
            ))
        }
    }
}

fn retry_after(header: Option<&str>) -> u64 {
    header
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(2)
        .min(30)
}

struct TeamsClient {
    base: String,
    token: String,
}

impl TeamsClient {
    fn get_json(&self, url: &str) -> Result<Value, TeamsError> {
        let full = if url.starts_with("http") { url.to_string() } else { format!("{}{url}", self.base) };
        let send = || {
            ureq::get(&full)
                .set("Authorization", &format!("Bearer {}", self.token))
                .timeout(HTTP_TIMEOUT)
                .call()
        };
        match send() {
            Ok(r) => r
                .into_json()
                .map_err(|e| TeamsError::Other(format!("json parse: {e}"))),
            Err(ureq::Error::Status(429, r)) => {
                std::thread::sleep(Duration::from_secs(retry_after(r.header("Retry-After"))));
                match send() {
                    Ok(r) => r
                        .into_json()
                        .map_err(|e| TeamsError::Other(format!("json parse: {e}"))),
                    Err(ureq::Error::Status(c, r)) => Err(classify(c, r, "")),
                    Err(e) => Err(TeamsError::Other(e.to_string())),
                }
            }
            Err(ureq::Error::Status(c, r)) => Err(classify(c, r, "")),
            Err(e) => Err(TeamsError::Other(e.to_string())),
        }
    }

    /// Drain all pages from a URL, collecting `value[]` items into one vec.
    fn drain_pages(&self, start_url: &str) -> Result<Vec<Value>, TeamsError> {
        let mut items: Vec<Value> = Vec::new();
        let mut url = start_url.to_string();
        loop {
            let v = self.get_json(&url)?;
            if let Some(arr) = v.get("value").and_then(Value::as_array) {
                items.extend(arr.iter().cloned());
            }
            match v.get("@odata.nextLink").and_then(Value::as_str).map(str::to_string) {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(items)
    }

    /// List the user's chats.
    fn list_chats(&self) -> Result<Vec<TeamsChatInfo>, TeamsError> {
        let raw = self.drain_pages("/me/chats?$select=id,topic")?;
        Ok(raw
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect())
    }

    /// Fetch messages in one chat, optionally filtered to those after `since`.
    /// Returns raw Graph chatMessage objects in ascending chronological order.
    ///
    /// **Graph API constraints:**
    /// - `$orderby=createdDateTime` only supports *descending* (`desc`) order;
    ///   ascending is rejected with HTTP 400.
    /// - `$filter` on `createdDateTime` only supports the `lt` operator, not
    ///   `gt`; using `gt` returns HTTP 400.
    ///
    /// Strategy: request `$orderby=createdDateTime desc` (newest-first), drain
    /// pages, and stop early once we encounter a message whose
    /// `createdDateTime` is at-or-before `since` (client-side watermark cut).
    /// After draining, reverse the collected slice back to ascending order
    /// before returning.
    fn list_messages(&self, chat_id: &str, since: Option<&str>) -> Result<Vec<Value>, TeamsError> {
        let start_url = format!(
            "/me/chats/{chat_id}/messages?\
             $orderby=createdDateTime+desc&\
             $top=50"
        );

        let mut items: Vec<Value> = Vec::new();
        let mut url = start_url;

        'pages: loop {
            let v = self.get_json(&url)?;
            if let Some(arr) = v.get("value").and_then(Value::as_array) {
                for msg in arr {
                    let created = msg
                        .get("createdDateTime")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // If we have a watermark and this message is at-or-before
                    // it, all subsequent messages (older) are already stored —
                    // stop paging.
                    if let Some(ts) = since {
                        if !created.is_empty() && created <= ts {
                            break 'pages;
                        }
                    }
                    items.push(msg.clone());
                }
            }
            match v.get("@odata.nextLink").and_then(Value::as_str).map(str::to_string) {
                Some(next) => url = next,
                None => break,
            }
        }

        // Reverse to ascending chronological order for consistent vault write.
        items.reverse();
        Ok(items)
    }
}

// ---------------------------------------------------------------------------
// Message conversion (pure — fixture tested).

/// Extract plain text from a Graph `body` object: prefer `text` content-type;
/// strip HTML tags for `html` content. Returns empty string for null/missing.
pub(crate) fn body_text(body: &Value) -> String {
    let content = body.get("content").and_then(Value::as_str).unwrap_or("");
    let content_type = body.get("contentType").and_then(Value::as_str).unwrap_or("text");
    if content_type == "html" {
        // Minimal HTML-strip: remove tags for vault storage. The raw layer
        // always preserves the full HTML.
        strip_html(content)
    } else {
        content.to_string()
    }
}

/// Strip HTML tags. Lightweight — only for vault text, not rendered display.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // Collapse whitespace.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Convert a raw Graph chatMessage object into a correspondence [`Message`].
/// Returns `None` for system/event messages or messages with no usable content.
/// PURE — fixture tested.
pub(crate) fn graph_message_to_correspondence(
    v: &Value,
    chat_id: &str,
    chat_name: &str,
    account_email: &str,
    my_user_id: Option<&str>,
) -> Option<Message> {
    let id = v.get("id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    // Skip system events and typing indicators — only "message" type.
    let msg_type = v.get("messageType").and_then(Value::as_str).unwrap_or("message");
    if msg_type != "message" {
        return None;
    }
    // Skip deleted messages.
    if v.get("deletedDateTime").and_then(Value::as_str).is_some() {
        return None;
    }
    let created = v.get("createdDateTime").and_then(Value::as_str)?;
    // Convert Graph UTC datetime to local RFC3339.
    let ts = utc_to_local_rfc3339(created)?;

    let body = v.get("body").cloned().unwrap_or(Value::Null);
    let text = body_text(&body);

    let attachments: Vec<AttachmentMeta> = v
        .get("attachments")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    if name.is_empty() {
                        None
                    } else {
                        Some(AttachmentMeta {
                            name,
                            mime: a
                                .get("contentType")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            bytes: 0,
                        })
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // Only store messages with content.
    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let from_user_id = v
        .get("from")
        .and_then(|f| f.get("user"))
        .and_then(|u| u.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let from_display = v
        .get("from")
        .and_then(|f| f.get("user"))
        .and_then(|u| u.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let from_me = my_user_id.is_some_and(|me| !me.is_empty() && me == from_user_id);

    let reply_to = v.get("replyToId").and_then(Value::as_str).unwrap_or("").to_string();

    let subject = v.get("subject").and_then(Value::as_str).unwrap_or("").to_string();

    let mut m = Message::new(SOURCE, ts);
    // Graph chatMessage `id` is unique only within a single chat/channel
    // (per the official docs: "IDs are unique within a chat/channel/
    // reply-to-message, but might be duplicated in other chats/channels/
    // reply-to-messages").  Scope the guid to the chat to prevent collision
    // across chats from silently dropping real messages.
    m.guid = format!("{chat_id}:{id}");
    m.chat = chat_id.to_string();
    m.chat_name = if !chat_name.is_empty() { chat_name.to_string() } else { String::new() };
    m.service = account_email.to_string();
    m.from_me = from_me;
    if !from_me {
        m.sender = from_user_id.to_string();
        m.sender_name = from_display.to_string();
    }
    m.text = text;
    m.attachments = attachments;
    m.subject = subject;
    m.reply_to = reply_to;
    Some(m)
}

/// Parse a UTC ISO datetime string (`"2021-03-28T20:48:29.832Z"`) to a local
/// RFC3339 string. Returns `None` when the input is unparseable.
pub(crate) fn utc_to_local_rfc3339(utc: &str) -> Option<String> {
    let t = DateTime::parse_from_rfc3339(utc)
        .or_else(|_| {
            // Graph sometimes emits ".832Z" as ".832000Z" etc; just try both.
            let fixed = utc.replace("Z", "+00:00");
            DateTime::parse_from_rfc3339(&fixed)
        })
        .ok()?;
    Some(t.with_timezone(&Local).to_rfc3339())
}

/// The newest `createdDateTime` in a batch of raw Graph message objects.
/// Returns `None` when the batch is empty or all items lack a timestamp.
pub(crate) fn newest_created_dt(items: &[Value]) -> Option<String> {
    items
        .iter()
        .filter_map(|v| v.get("createdDateTime").and_then(Value::as_str))
        .max()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// The collector.

#[derive(Debug, Default)]
struct TeamsStats {
    accounts: u32,
    chats: u32,
    messages: u64,
}

/// One Teams sync pass across all connected Microsoft accounts.
fn collect(vault: &Vault) -> Result<TeamsStats> {
    let accounts = crate::outlook::microsoft_accounts(vault)?;
    let mut state = vault.read_teams_sync();

    // Prune disconnected accounts from the cursor.
    let live: HashSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    state.accounts.retain(|id, _| live.contains(id.as_str()));

    if accounts.is_empty() {
        if vault.resolve(SYNC_FILE).map(|p| p.exists()).unwrap_or(false) {
            state.updated = Local::now().to_rfc3339();
            vault.write_teams_sync(&state)?;
        }
        return Ok(TeamsStats::default());
    }

    let mut seen = vault.correspondence_guids(SOURCE)?;
    let mut stats = TeamsStats::default();

    for acct in &accounts {
        if acct.needs_reconnect {
            continue;
        }
        let acct_state = state.accounts.entry(acct.id.clone()).or_default();
        acct_state.email = acct.email.clone();
        acct_state.error = None;

        let token = match crate::outlook::microsoft_fresh_token(vault, &acct.id) {
            Ok(t) => t,
            Err(e) => {
                acct_state.error = Some(format!("{e:#}"));
                continue;
            }
        };
        let client = TeamsClient { base: GRAPH_BASE.to_string(), token };

        match sync_account(vault, &client, &acct.id, &acct.email, &mut state, &mut seen) {
            Ok(s) => {
                if s.messages > 0 {
                    stats.accounts += 1;
                    stats.chats += s.chats;
                    stats.messages += s.messages;
                }
            }
            Err(e) => {
                let acct_state = state.accounts.entry(acct.id.clone()).or_default();
                acct_state.error = Some(e.to_string());
            }
        }
    }

    state.updated = Local::now().to_rfc3339();
    vault.write_teams_sync(&state)?;
    Ok(stats)
}

/// Sync one account: list chats, drain new messages from each.
fn sync_account(
    vault: &Vault,
    client: &TeamsClient,
    acct_id: &str,
    acct_email: &str,
    state: &mut SyncState,
    seen: &mut HashSet<String>,
) -> Result<TeamsStats, TeamsError> {
    // Resolve the user's own Graph id so we can mark from_me.
    let my_user_id = fetch_my_id(client).unwrap_or_default();

    let chats = client.list_chats()?;
    let mut stats = TeamsStats::default();

    for chat in chats {
        let acct_state = state.accounts.entry(acct_id.to_string()).or_default();
        acct_state.email = acct_email.to_string();
        let cursor = acct_state.chats.entry(chat.id.clone()).or_default();
        let since = cursor.since.clone();
        let chat_name = chat.topic.clone().unwrap_or_default();
        cursor.label = chat_name.clone();

        let raw_items = match client.list_messages(&chat.id, since.as_deref()) {
            Ok(items) => items,
            Err(TeamsError::Unauthorized) => return Err(TeamsError::Unauthorized),
            Err(_) => {
                // Per-chat failure: record and continue to next chat.
                let acct_state = state.accounts.entry(acct_id.to_string()).or_default();
                let c = acct_state.chats.entry(chat.id.clone()).or_default();
                c.label = chat_name.clone();
                continue;
            }
        };

        if raw_items.is_empty() {
            continue;
        }

        // Persist raw layer unconditionally.
        let raw_stream = vault.stream(RAW_DIR, Partition::Month);
        let _ = raw_stream.append(&raw_items, |v| {
            v.get("createdDateTime").and_then(Value::as_str).unwrap_or("")
        });

        // Convert to contract rows, deduped by guid.
        let mut batch: Vec<Message> = Vec::new();
        for v in &raw_items {
            let Some(m) = graph_message_to_correspondence(
                v,
                &chat.id,
                &chat_name,
                acct_email,
                Some(&my_user_id),
            ) else {
                continue;
            };
            if seen.insert(m.guid.clone()) {
                batch.push(m);
            }
        }

        // Advance cursor only after successful write.
        let written = batch.len() as u64;
        if !batch.is_empty() {
            vault.append_messages(&batch).map_err(|e| TeamsError::Other(format!("{e:#}")))?;
        }

        // Advance watermark: the newest `createdDateTime` in the raw batch.
        if let Some(newest) = newest_created_dt(&raw_items) {
            let acct_state = state.accounts.entry(acct_id.to_string()).or_default();
            let c = acct_state.chats.entry(chat.id.clone()).or_default();
            c.since = Some(newest);
            c.messages += written;
        }
        vault
            .write_teams_sync(state)
            .map_err(|e| TeamsError::Other(format!("{e:#}")))?;

        if written > 0 {
            stats.chats += 1;
            stats.messages += written;
        }
    }
    Ok(stats)
}

/// Resolve the signed-in user's own Graph id for from_me detection. A failure
/// here is non-fatal — from_me stays false for all messages.
fn fetch_my_id(client: &TeamsClient) -> Option<String> {
    let v = client.get_json("/me?$select=id").ok()?;
    v.get("id").and_then(Value::as_str).map(str::to_string)
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/microsoft-teams"))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    if crate::outlook::microsoft_accounts(vault).map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    match collect(vault) {
        Ok(s) => Ok(crate::registry::CollectOutcome::note_if(s.messages > 0, || {
            format!(
                "teams synced — {} messages across {} chats, {} accounts",
                s.messages, s.chats, s.accounts
            )
        })),
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("teams sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    if crate::outlook::microsoft_accounts(vault)?.is_empty() {
        bail!("no Microsoft account is connected");
    }
    let s = collect(vault)?;
    let errors = vault
        .read_teams_sync()
        .accounts
        .values()
        .filter(|a| a.error.is_some())
        .count() as u64;
    let mut headline = if s.messages > 0 {
        format!("{} new messages across {} chats in {} accounts", s.messages, s.chats, s.accounts)
    } else {
        "no new messages".to_string()
    };
    if errors > 0 {
        headline.push_str(&format!(
            " — {errors} account{} failed",
            if errors == 1 { "" } else { "s" }
        ));
    }
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("accounts", s.accounts as u64),
            ("chats", s.chats as u64),
            ("messages", s.messages),
            ("account_errors", errors),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (replaces the NotWired
/// stub). Reuses the "microsoft" connection from [`crate::outlook`].
///
/// **NOTE:** The "microsoft" OAuth provider currently bundles
/// `Mail.Read Calendars.Read User.Read offline_access`. Teams chat requires
/// `Chat.Read` — that scope must be added to `crate::outlook::MICROSOFT` and
/// granted in the Entra app registration before live data can flow.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "microsoft-teams",
        name: "Microsoft Teams",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls Microsoft Teams DMs and group chats via the Microsoft Graph API \
                      into the correspondence stream. Shares one login with Outlook, \
                      Calendar, and To Do. Requires Chat.Read scope on the connected \
                      Microsoft account (added to the Entra app alongside Mail.Read).",
        domain: "correspondence",
        vault_path: "correspondence/microsoft-teams/",
        toggleable: true,
        setup: &[
            "Connect a Microsoft account on the Microsoft card above (shared with Outlook).",
            "Ensure Chat.Read is granted in your Entra app registration (Azure → App registrations → API permissions).",
            "Sync now; Teams DMs and group chats flow into the correspondence stream.",
        ],
        caveats: "Requires a work or school (Azure AD) Microsoft account — personal Outlook.com \
                 accounts do not have access to the Teams Graph endpoints. Channel messages \
                 require admin-granted ChannelMessage.Read.All and are not included by default. \
                 Message bodies are stored as plain text; the meeting-transcript slice ships \
                 after the meetings contract ratifies.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(TEAMS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("microsoft"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-msteams-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Timestamp parsing.

    #[test]
    fn utc_to_local_rfc3339_parses_graph_timestamps() {
        // Standard Graph UTC timestamps.
        let ts = utc_to_local_rfc3339("2021-03-28T20:48:29.832Z").unwrap();
        assert!(ts.len() >= 19, "returned RFC3339: {ts}");
        assert!(!ts.is_empty());

        let ts2 = utc_to_local_rfc3339("2021-03-17T08:59:08.136Z").unwrap();
        assert!(ts2.starts_with("2021-"), "year preserved: {ts2}");

        assert!(utc_to_local_rfc3339("").is_none());
        assert!(utc_to_local_rfc3339("not-a-date").is_none());
    }

    // -----------------------------------------------------------------------
    // HTML stripping.

    #[test]
    fn strip_html_removes_tags_and_collapses_whitespace() {
        assert_eq!(strip_html("<div>Hello <b>world</b></div>"), "Hello world");
        assert_eq!(strip_html("plain text"), "plain text");
        assert_eq!(strip_html(""), "");
        assert_eq!(strip_html("<systemEventMessage/>"), "");
        // Adjacent div tags with no whitespace between them merge their content.
        // Raw HTML is preserved in the raw layer so no content is lost.
        assert_eq!(
            strip_html("<div>line one</div> <div>line two</div>"),
            "line one line two"
        );
    }

    // -----------------------------------------------------------------------
    // body_text: text vs html content-type.

    #[test]
    fn body_text_returns_plain_text_for_text_content_type() {
        let body = serde_json::json!({"contentType": "text", "content": "Hello world"});
        assert_eq!(body_text(&body), "Hello world");
    }

    #[test]
    fn body_text_strips_html_for_html_content_type() {
        let body =
            serde_json::json!({"contentType": "html", "content": "<div><b>Hello</b></div>"});
        assert_eq!(body_text(&body), "Hello");
    }

    #[test]
    fn body_text_returns_empty_for_null() {
        assert_eq!(body_text(&Value::Null), "");
        assert_eq!(
            body_text(&serde_json::json!({"contentType": "text", "content": ""})),
            ""
        );
    }

    // -----------------------------------------------------------------------
    // graph_message_to_correspondence: fixture from the official Graph docs.

    fn sample_message() -> Value {
        serde_json::json!({
            "id": "1616964509832",
            "replyToId": null,
            "messageType": "message",
            "createdDateTime": "2021-03-28T20:48:29.832Z",
            "deletedDateTime": null,
            "subject": null,
            "chatId": "19:2da4c29f6d7041eca70b638b43d45437@thread.v2",
            "from": {
                "user": {
                    "id": "8ea0e38b-efb3-4757-924a-5f94061cf8c2",
                    "displayName": "Robin Kline",
                    "userIdentityType": "aadUser"
                }
            },
            "body": {
                "contentType": "text",
                "content": "Hello world"
            },
            "attachments": [],
            "mentions": [],
            "reactions": []
        })
    }

    #[test]
    fn converts_graph_message_to_correspondence_row() {
        let v = sample_message();
        let m = graph_message_to_correspondence(
            &v,
            "19:2da4c29f6d7041eca70b638b43d45437@thread.v2",
            "My Chat",
            "me@example.com",
            Some("different-user-id"),
        )
        .unwrap();
        assert_eq!(m.source, "microsoft-teams");
        // guid is chat-scoped: "{chat_id}:{message_id}" to prevent cross-chat
        // collisions (Graph message ids are only unique within a chat).
        assert_eq!(m.guid, "19:2da4c29f6d7041eca70b638b43d45437@thread.v2:1616964509832");
        assert_eq!(m.chat, "19:2da4c29f6d7041eca70b638b43d45437@thread.v2");
        assert_eq!(m.chat_name, "My Chat");
        assert_eq!(m.service, "me@example.com");
        assert_eq!(m.text, "Hello world");
        assert!(!m.from_me, "different sender → not from_me");
        assert_eq!(m.sender, "8ea0e38b-efb3-4757-924a-5f94061cf8c2");
        assert_eq!(m.sender_name, "Robin Kline");
        assert!(m.ts.contains("2021-"), "ts round-trips: {}", m.ts);
    }

    #[test]
    fn from_me_set_when_user_ids_match() {
        let v = sample_message();
        let m = graph_message_to_correspondence(
            &v,
            "chat-id",
            "Chat",
            "me@example.com",
            Some("8ea0e38b-efb3-4757-924a-5f94061cf8c2"),
        )
        .unwrap();
        assert!(m.from_me, "matching user id → from_me");
        assert_eq!(m.sender, "", "sender empty when from_me");
        assert_eq!(m.sender_name, "", "sender_name empty when from_me");
    }

    #[test]
    fn skips_system_event_messages() {
        let v = serde_json::json!({
            "id": "1615943825123",
            "messageType": "systemEventMessage",
            "createdDateTime": "2021-03-17T06:47:05.123Z",
            "deletedDateTime": null,
            "from": null,
            "body": {"contentType": "html", "content": "<systemEventMessage/>"},
            "attachments": []
        });
        assert!(
            graph_message_to_correspondence(&v, "chat", "", "me@example.com", None).is_none(),
            "system event messages must be skipped"
        );
    }

    #[test]
    fn skips_deleted_messages() {
        let mut v = sample_message();
        v["deletedDateTime"] = serde_json::json!("2021-04-01T12:00:00.000Z");
        assert!(
            graph_message_to_correspondence(&v, "chat", "", "me@example.com", None).is_none(),
            "deleted messages must be skipped"
        );
    }

    #[test]
    fn skips_messages_with_no_content_and_no_attachments() {
        let v = serde_json::json!({
            "id": "empty-msg",
            "messageType": "message",
            "createdDateTime": "2021-03-28T20:48:29.832Z",
            "deletedDateTime": null,
            "from": {"user": {"id": "u1", "displayName": "A"}},
            "body": {"contentType": "text", "content": ""},
            "attachments": []
        });
        assert!(
            graph_message_to_correspondence(&v, "chat", "", "me@example.com", None).is_none(),
            "empty body + no attachments = skipped"
        );
    }

    #[test]
    fn includes_message_with_attachments_and_no_text() {
        let v = serde_json::json!({
            "id": "attach-only",
            "messageType": "message",
            "createdDateTime": "2021-03-28T20:48:29.832Z",
            "deletedDateTime": null,
            "from": {"user": {"id": "u1", "displayName": "A"}},
            "body": {"contentType": "text", "content": ""},
            "attachments": [{"name": "report.pdf", "contentType": "application/pdf"}]
        });
        let m = graph_message_to_correspondence(&v, "chat", "", "me@example.com", None).unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].name, "report.pdf");
    }

    // -----------------------------------------------------------------------
    // newest_created_dt.

    #[test]
    fn newest_created_dt_picks_the_latest() {
        let items = vec![
            serde_json::json!({"createdDateTime": "2021-03-17T08:59:08.136Z"}),
            serde_json::json!({"createdDateTime": "2021-03-28T20:48:29.832Z"}),
            serde_json::json!({"createdDateTime": "2021-03-15T00:00:00.000Z"}),
        ];
        let newest = newest_created_dt(&items).unwrap();
        assert_eq!(newest, "2021-03-28T20:48:29.832Z");
    }

    #[test]
    fn newest_created_dt_empty_batch_returns_none() {
        assert!(newest_created_dt(&[]).is_none());
    }

    // -----------------------------------------------------------------------
    // End-to-end: parse a batch → correspondence rows → written to vault.

    #[test]
    fn batch_of_messages_lands_in_correspondence_stream() {
        let v = temp_vault("batch");
        let msgs = vec![
            serde_json::json!({
                "id": "msg-001",
                "messageType": "message",
                "createdDateTime": "2026-06-10T14:00:00.000Z",
                "deletedDateTime": null,
                "from": {
                    "user": {
                        "id": "user-alice",
                        "displayName": "Alice",
                        "userIdentityType": "aadUser"
                    }
                },
                "body": {"contentType": "text", "content": "Hey Dave"},
                "attachments": []
            }),
            serde_json::json!({
                "id": "msg-002",
                "messageType": "message",
                "createdDateTime": "2026-06-10T14:05:00.000Z",
                "deletedDateTime": null,
                "from": {
                    "user": {
                        "id": "user-dave",
                        "displayName": "Dave",
                        "userIdentityType": "aadUser"
                    }
                },
                "body": {"contentType": "text", "content": "Hey Alice!"},
                "attachments": []
            }),
            // system event — must be skipped
            serde_json::json!({
                "id": "sys-001",
                "messageType": "systemEventMessage",
                "createdDateTime": "2026-06-10T13:00:00.000Z",
                "deletedDateTime": null,
                "from": null,
                "body": {"contentType": "html", "content": "<systemEventMessage/>"},
                "attachments": []
            }),
        ];

        let mut seen = v.correspondence_guids(SOURCE).unwrap();
        let mut batch: Vec<Message> = Vec::new();
        for raw in &msgs {
            if let Some(m) = graph_message_to_correspondence(
                raw,
                "chat-abc",
                "Side Project",
                "dave@example.com",
                Some("user-dave"),
            ) {
                if seen.insert(m.guid.clone()) {
                    batch.push(m);
                }
            }
        }

        assert_eq!(batch.len(), 2, "system event was skipped");
        v.append_messages(&batch).unwrap();

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);

        let alice_msg = day.iter().find(|m| m.guid == "chat-abc:msg-001").unwrap();
        assert_eq!(alice_msg.source, "microsoft-teams");
        assert_eq!(alice_msg.chat, "chat-abc");
        assert_eq!(alice_msg.chat_name, "Side Project");
        assert_eq!(alice_msg.text, "Hey Dave");
        assert!(!alice_msg.from_me);
        assert_eq!(alice_msg.sender, "user-alice");
        assert_eq!(alice_msg.sender_name, "Alice");

        let dave_msg = day.iter().find(|m| m.guid == "chat-abc:msg-002").unwrap();
        assert!(dave_msg.from_me, "dave is the account owner");
        assert_eq!(dave_msg.text, "Hey Alice!");

        // Vault file lives at correspondence/microsoft-teams/<month>.jsonl, NOT email/.
        assert!(v.root().join("correspondence/microsoft-teams/2026-06.jsonl").exists());
        assert!(!v.root().join("correspondence/email/2026-06.jsonl").exists());
    }

    // -----------------------------------------------------------------------
    // Cursor: SyncState round-trips and first-run has no `since`.

    #[test]
    fn cursor_round_trips_and_first_run_has_no_since() {
        let v = temp_vault("cursor");
        let state = v.read_teams_sync();
        assert!(state.accounts.is_empty(), "fresh vault → empty cursor");

        let mut s = SyncState::default();
        {
            let acct = s.accounts.entry("acct-1".into()).or_default();
            acct.email = "alice@example.com".into();
            let chat = acct.chats.entry("chat-1".into()).or_default();
            chat.since = Some("2021-03-28T20:48:29.832Z".into());
            chat.messages = 5;
        }
        v.write_teams_sync(&s).unwrap();

        let s2 = v.read_teams_sync();
        let acct2 = s2.accounts.get("acct-1").unwrap();
        assert_eq!(acct2.email, "alice@example.com");
        let chat2 = acct2.chats.get("chat-1").unwrap();
        assert_eq!(chat2.since.as_deref(), Some("2021-03-28T20:48:29.832Z"));
        assert_eq!(chat2.messages, 5);

        // A new chat has no since (first run).
        let chat_new = acct2.chats.get("chat-new");
        assert!(chat_new.is_none());
    }

    // -----------------------------------------------------------------------
    // Deduplication: the same guid is never written twice.

    #[test]
    fn duplicate_guid_is_skipped_on_re_import() {
        let v = temp_vault("dedup");
        let raw = serde_json::json!({
            "id": "dup-msg",
            "messageType": "message",
            "createdDateTime": "2026-06-10T14:00:00.000Z",
            "deletedDateTime": null,
            "from": {"user": {"id": "u1", "displayName": "Bob"}},
            "body": {"contentType": "text", "content": "Hi"},
            "attachments": []
        });

        let mut seen = v.correspondence_guids(SOURCE).unwrap();
        let m = graph_message_to_correspondence(&raw, "chat", "", "me@example.com", None).unwrap();
        assert!(seen.insert(m.guid.clone()), "first insert → true");
        v.append_messages(&[m.clone()]).unwrap();

        // Second pass: guid already in seen → skipped.
        let seen2 = v.correspondence_guids(SOURCE).unwrap();
        assert!(seen2.contains("chat:dup-msg"), "guid is in the stored set (chat-scoped)");
        let m2 = graph_message_to_correspondence(&raw, "chat", "", "me@example.com", None).unwrap();
        let mut s2 = seen2;
        assert!(!s2.insert(m2.guid.clone()), "duplicate → not inserted");

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 1, "only one row despite two attempts");
    }

    // -----------------------------------------------------------------------
    // Old vault lines still deserialise (back-compat proof).

    #[test]
    fn old_correspondence_lines_still_deserialise() {
        let line = r#"{"ts":"2026-06-10T14:00:00-07:00","source":"microsoft-teams","chat":"chat-1","from_me":false,"kind":"message","text":"Hello"}"#;
        let m: crate::correspondence::Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.source, "microsoft-teams");
        assert_eq!(m.text, "Hello");
        assert!(!m.from_me);
    }

    // -----------------------------------------------------------------------
    // HTTP-mocked: list_messages URL contract and watermark cut.
    //
    // Spins up a real TCP server on an ephemeral port (127.0.0.1:0) so the
    // actual ureq call executes and the URL Graph receives is observable.

    /// Start a minimal HTTP/1.1 stub on an OS-assigned port.
    /// `router` is called with the request-target string; returns (status, body).
    fn stub_server<F>(router: F) -> String
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Arc::new(router);
        std::thread::spawn(move || {
            // Serve a bounded number of connections (one per request).
            for stream in listener.incoming().take(20) {
                let Ok(mut s) = stream else { break };
                let router = Arc::clone(&router);
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    loop {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                head.extend_from_slice(&buf[..n]);
                                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let req = String::from_utf8_lossy(&head);
                    let target = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    let (status, body) = router(&target);
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        format!("http://{addr}")
    }

    fn msg_json(id: &str, created: &str, content: &str) -> Value {
        serde_json::json!({
            "id": id,
            "messageType": "message",
            "createdDateTime": created,
            "deletedDateTime": null,
            "from": {"user": {"id": "user-1", "displayName": "Alice"}},
            "body": {"contentType": "text", "content": content},
            "attachments": []
        })
    }

    /// Verifies that list_messages uses `$orderby=createdDateTime+desc` (NOT
    /// asc) and does NOT use a `$filter=createdDateTime+gt+...` clause — both
    /// of which are rejected by the real Graph API with HTTP 400.
    ///
    /// Also verifies the client-side watermark cut: messages at-or-before
    /// `since` are excluded from the result.
    #[test]
    fn list_messages_uses_desc_order_and_client_side_watermark_cut() {
        // Three messages in descending order (as Graph would return them).
        let pages: Vec<(u16, String)> = vec![(
            200,
            serde_json::json!({
                "value": [
                    msg_json("msg-3", "2021-04-01T10:00:00.000Z", "third"),
                    msg_json("msg-2", "2021-03-29T09:00:00.000Z", "second"),
                    // msg-1 is AT the watermark — should be excluded (<=).
                    msg_json("msg-1", "2021-03-28T20:48:29.832Z", "first"),
                ]
            })
            .to_string(),
        )];

        // Track what URLs the stub received.
        let received_urls: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received_urls);
        let pages = Arc::new(std::sync::Mutex::new(pages.into_iter()));

        let base = stub_server(move |target: &str| {
            received_clone.lock().unwrap().push(target.to_string());
            if let Some((status, body)) = pages.lock().unwrap().next() {
                return (status, body);
            }
            (200, serde_json::json!({"value": []}).to_string())
        });

        let client = TeamsClient { base, token: "test-token".into() };
        // since = the createdDateTime of msg-1; only msg-2 and msg-3 should
        // be returned (msg-1 is at-or-before the watermark).
        let since = "2021-03-28T20:48:29.832Z";
        let result = client.list_messages("chat-xyz", Some(since)).unwrap();

        // URL must use desc order and must NOT contain `$filter` or `asc`.
        let urls = received_urls.lock().unwrap().clone();
        assert!(!urls.is_empty(), "stub must have received at least one request");
        let first_url = &urls[0];
        assert!(
            first_url.contains("orderby=createdDateTime+desc")
                || first_url.contains("orderby=createdDateTime%20desc"),
            "must request descending order (Graph only supports desc for createdDateTime); got: {first_url}"
        );
        assert!(
            !first_url.contains("asc"),
            "must NOT request ascending order (unsupported by Graph); got: {first_url}"
        );
        assert!(
            !first_url.contains("$filter"),
            "must NOT use $filter on createdDateTime (gt is unsupported by Graph); got: {first_url}"
        );

        // Client-side watermark cut: msg-1 (== since) is excluded.
        assert_eq!(result.len(), 2, "only messages AFTER the watermark are returned");

        // Result should be in ascending order (reversed after drain).
        let ids: Vec<_> = result
            .iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["msg-2", "msg-3"], "ascending chronological order after reversal");
    }

    /// First-run (no `since`): all messages are returned.
    #[test]
    fn list_messages_first_run_returns_all_messages() {
        let base = stub_server(|_target| {
            (
                200,
                serde_json::json!({
                    "value": [
                        msg_json("msg-b", "2021-04-01T10:00:00.000Z", "B"),
                        msg_json("msg-a", "2021-03-28T20:00:00.000Z", "A"),
                    ]
                })
                .to_string(),
            )
        });

        let client = TeamsClient { base, token: "test-token".into() };
        let result = client.list_messages("chat-xyz", None).unwrap();

        assert_eq!(result.len(), 2, "all messages returned on first run");
        // Ascending order after reversal.
        let ids: Vec<_> = result
            .iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["msg-a", "msg-b"]);
    }
}
