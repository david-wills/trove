//! Fastmail — incremental email pull via the JMAP API (RFC 8620 + 8621).
//!
//! **Target.** Messages land in `correspondence/email/YYYY-MM.jsonl` (see
//! [`crate::correspondence`]) — the *same* stream the `.mbox` importer
//! ([`crate::email`]), the Gmail puller ([`crate::gmail`]), and the generic
//! IMAP collector ([`crate::imap`]) write — sharing their Message-ID-based
//! `guid` dedupe. A Fastmail address can be reached via the generic IMAP
//! collector first and this dedicated JMAP provider later (or vice-versa)
//! without duplication.
//!
//! **Why JMAP over IMAP?** JMAP's `Email/changes` cursor is standards-defined,
//! server-native, and JSON-over-HTTPS — no TLS socket management, no UIDVALIDITY
//! dance, no folder enumeration. One session fetch seeds the account id; then a
//! `Email/query` backfill (with a page cursor) or `Email/changes` incremental
//! keeps the vault current. The JMAP `receivedAt` is server-canonical (no
//! envelope-date timezone edge cases).
//!
//! **Auth.** A Fastmail API token (from Settings → Privacy & Security →
//! API tokens) stored in the 0600 secret store via a
//! [`crate::registry::ConnectMethod::TokenPaste`] connection. The token is
//! submitted as `Authorization: Bearer <token>` on every request.
//!
//! **Two phases, resumable:**
//!
//! 1. **Backfill** — `Email/query` with `sort=[{property:"receivedAt"}]`,
//!    paging via an `anchor`/`position` cursor persisted per page. The JMAP
//!    state is captured *before* the first page so `Email/changes` won't miss
//!    mail arriving during a long backfill.
//! 2. **Incremental** — `Email/changes` from the stored JMAP state token. A
//!    `cannotCalculateChanges` triggers a clean re-backfill (guid dedupe
//!    makes the re-fetch cheap).
//!
//! **Conversion.** We ask `Email/get` for structured header fields
//! (`messageId`, `from`, `to`, `cc`, `subject`, `textBody`, `hasAttachment`,
//! `receivedAt`, `mailboxIds`) and build a [`crate::correspondence::Message`]
//! directly — no raw RFC822 download needed, no extra blob-fetch round-trip.
//! Mailbox names are resolved via a `Mailbox/get` at session start and cached
//! for the pass. The `labels` field carries the mailbox names (paralleling how
//! Gmail's `labelIds` work). `guid` = the first `messageId` header value (i.e.
//! the RFC 5322 `Message-ID`), or a content-hash fallback for messages without
//! one, matching the dedupe key every other email collector uses.
//!
//! **Cursor.** Non-secret JMAP state string persisted at
//! `.trove/fastmail-sync.json`. Deleting it re-runs the full backfill (guid
//! dedupe absorbs the overlap).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::correspondence::Message;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::write_json_atomic;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

/// The service id under `.trove/sync/` where the API token is stored.
const SERVICE: &str = "fastmail";
/// Non-secret rebuildable cursor.
const SYNC_FILE: &str = ".trove/fastmail-sync.json";
/// Fastmail JMAP session endpoint.
const SESSION_URL: &str = "https://api.fastmail.com/jmap/session";
/// How many emails to fetch per `Email/get` batch.
const BATCH_SIZE: u32 = 50;
/// How many emails to fetch per `Email/query` backfill page.
const PAGE_SIZE: u32 = 50;
/// HTTP timeout — kept short so a hung connection can't stall the watcher.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between JMAP incremental passes in the watcher loop.
pub const FASTMAIL_SYNC_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// Sync state (non-secret, rebuildable)

/// Per-account sync progress, persisted at `.trove/fastmail-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FastmailSyncState {
    /// The account email address (non-secret, for UI display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// JMAP state string for `Email/changes` (the incremental cursor).
    /// Captured before the first backfill page so nothing arriving mid-backfill
    /// is missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jmap_state: Option<String>,
    /// Whether the full backfill has completed.
    #[serde(default)]
    pub backfill_done: bool,
    /// `Email/query` backfill anchor: the JMAP id of the last email seen in the
    /// previous page. Resume uses `anchor + anchorOffset=1` which is stable
    /// against concurrent deletes (RFC 8620 §5.5). `None` = start from position 0.
    ///
    /// Note: the old `backfill_position` (numeric) field is intentionally
    /// absent — serde will silently skip any old JSON value for it, so existing
    /// cursors safely default to `backfill_anchor = None` (re-start backfill
    /// from the beginning, guid dedupe absorbs the overlap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill_anchor: Option<String>,
    /// Total messages written (all time).
    #[serde(default)]
    pub messages: u64,
    /// RFC3339 local time of the last sync attempt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Last error, for the hub status card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Registry face

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/email"))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    if load_token(vault)?.is_none() {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    match pull_inner(vault) {
        Ok(n) => Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
            format!("fastmail synced — {n} messages")
        })),
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "fastmail sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let n = pull_inner(vault)?;
    Ok(PullOutcome {
        headline: if n == 0 {
            "Fastmail is up to date — no new messages".to_string()
        } else {
            format!("Fastmail synced — {n} new messages")
        },
        counts: BTreeMap::from([("messages", n)]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (replaces the NotWired
/// stub). The `pub mod fastmail;` and the INTEGRATIONS `&crate::fastmail::DEF`
/// line ALREADY exist — do not add them again.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "fastmail",
        name: "Fastmail",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Fastmail email incrementally using the JMAP API \
                      (RFC 8621) into the same email stream as Gmail and mbox imports, \
                      deduped by Message-ID. Backfills on connect, then keeps current \
                      via Fastmail's server-native change cursor.",
        domain: "correspondence",
        vault_path: "correspondence/email/",
        toggleable: true,
        setup: &[
            "Generate an API token: Fastmail → Settings → Privacy & Security → API tokens → New token → Mail (read).",
            "Connect with that token on this card.",
            "First sync backfills your full mailbox; later syncs are incremental.",
        ],
        caveats: "Reads full message text and attachment metadata — connect early, \
                  because mail the server has already deleted is gone before Trove \
                  can save it. Raw .eml archives and attachment files are not stored \
                  (a later opt-in). Trove reads all mailboxes except Trash and Spam.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(FASTMAIL_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("fastmail"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = Fastmail API token)

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty API token — generate one under Settings → Privacy & Security → API tokens");
    }
    // Validate: probe the session endpoint. A clear error reaches the connect UI.
    let session = fetch_session(SESSION_URL, token)
        .context("checking the Fastmail API token — make sure it has Mail (read) scope")?;
    let email = session_primary_email(&session);
    // Store the token in the 0600 secret store.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        },
    )?;
    // Record the (non-secret) address on the cursor.
    let mut state = vault.read_fastmail_sync();
    state.email = email;
    vault.write_fastmail_sync(&state)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Ok(Some(tok)) = vault.load_sync_token(SERVICE) {
        let state = vault.read_fastmail_sync();
        let label = state
            .email
            .clone()
            .unwrap_or_else(|| tok.access_token.chars().take(8).collect::<String>() + "…");
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

/// Registered in [`crate::integrations::CONNECTIONS`] (the integrator adds one
/// `&crate::fastmail::CONNECTION,` line there).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "fastmail",
    display_name: "Fastmail",
    methods: &[ConnectMethod::TokenPaste {
        label: "API token",
        help: "Generate a Fastmail API token: Settings → Privacy & Security → API tokens → \
               New token. Give it Mail (read) scope — you don't need other scopes for Trove. \
               The token looks like a long alphanumeric string. Paste it here.",
        placeholder: "fmu1-…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["fastmail"],
    setup: &[
        "In Fastmail, go to Settings → Privacy & Security → API tokens → New token.",
        "Select the Mail (read) scope and give the token a name like \"Trove\".",
        "Copy the generated token and paste it here.",
    ],
};

// ---------------------------------------------------------------------------
// JMAP session

/// The parts of the JMAP session document we need.
#[derive(Debug, Clone)]
struct Session {
    /// JMAP API URL (from `apiUrl`).
    api_url: String,
    /// The primary account id.
    account_id: String,
}

/// Fetch and parse the JMAP session document.
fn fetch_session(session_url: &str, token: &str) -> Result<Value> {
    let resp = ureq::get(session_url)
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(HTTP_TIMEOUT)
        .call()
        .context("fetching JMAP session")?;
    resp.into_json::<Value>().context("parsing JMAP session JSON")
}

fn parse_session(v: &Value) -> Result<Session> {
    let api_url = v
        .get("apiUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("JMAP session missing apiUrl"))?
        .to_string();
    // The primaryAccounts object maps capability URIs to account ids.
    // We prefer the JMAP mail capability; fall back to any account.
    let account_id = v
        .get("primaryAccounts")
        .and_then(|pa| {
            pa.get("urn:ietf:params:jmap:mail")
                .or_else(|| pa.get("urn:ietf:params:jmap:core"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            v.get("accounts")
                .and_then(Value::as_object)
                .and_then(|o| o.keys().next())
                .map(String::as_str)
        })
        .ok_or_else(|| anyhow::anyhow!("JMAP session: no usable accountId"))?
        .to_string();
    Ok(Session { api_url, account_id })
}

/// Extract the primary email address from a session document (best effort).
fn session_primary_email(session: &Value) -> Option<String> {
    // The `accounts` object's value has a `name` field which is usually the email.
    session
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|o| o.values().next())
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// JMAP API client

/// Post one JMAP method-call batch and return the `methodResponses` array.
fn jmap_call(session: &Session, token: &str, calls: Value) -> Result<Value> {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": calls,
    });
    let resp = ureq::post(&session.api_url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send_json(body)
        .context("JMAP API call")?;
    let v: Value = resp.into_json().context("parsing JMAP response")?;
    v.get("methodResponses")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("JMAP response missing methodResponses"))
}

/// Mailbox metadata: display name + optional JMAP role.
#[derive(Debug, Clone)]
struct Mailbox {
    name: String,
    /// JMAP role from RFC 8621 §2 (e.g. `"trash"`, `"junk"`, `"sent"`, `"inbox"`, …).
    role: Option<String>,
}

/// Fetch all mailboxes (name + role) keyed by mailbox id.
///
/// We request `role` so we can identify Trash (`role == "trash"`) and Spam/Junk
/// (`role == "junk"`) mailboxes and exclude them from the email query.  RFC 8621
/// §2 defines these standard roles; Fastmail honours them.
fn fetch_mailboxes(
    session: &Session,
    token: &str,
) -> Result<BTreeMap<String, Mailbox>> {
    let calls = json!([[
        "Mailbox/get",
        {
            "accountId": session.account_id,
            "ids": null,
            "properties": ["id", "name", "role"],
        },
        "m0"
    ]]);
    let responses = jmap_call(session, token, calls)?;
    let mut out = BTreeMap::new();
    if let Some(arr) = responses.as_array() {
        for resp in arr {
            let args = resp.get(1).unwrap_or(&Value::Null);
            if let Some(list) = args.get("list").and_then(Value::as_array) {
                for mb in list {
                    let id = mb.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let name = mb.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    if !id.is_empty() && !name.is_empty() {
                        let role = mb
                            .get("role")
                            .and_then(Value::as_str)
                            .map(str::to_lowercase);
                        out.insert(id, Mailbox { name, role });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Fetch details for a slice of email ids. Returns a list of parsed Messages
/// (None entries are silently dropped — unparseable / undateable messages).
fn fetch_email_details(
    session: &Session,
    token: &str,
    ids: &[&str],
    mailboxes: &BTreeMap<String, Mailbox>,
    account_email: &str,
) -> Result<Vec<Message>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let calls = json!([[
        "Email/get",
        {
            "accountId": session.account_id,
            "ids": ids,
            "properties": [
                "id",
                "receivedAt",
                "messageId",
                "inReplyTo",
                "from",
                "to",
                "cc",
                "subject",
                "textBody",
                "bodyValues",
                "hasAttachment",
                "mailboxIds",
                "keywords",
            ],
            "fetchTextBodyValues": true,
            "maxBodyValueBytes": 131072u64,
        },
        "e0"
    ]]);
    let responses = jmap_call(session, token, calls)?;
    let mut out = Vec::new();
    if let Some(arr) = responses.as_array() {
        for resp in arr {
            let args = resp.get(1).unwrap_or(&Value::Null);
            if let Some(list) = args.get("list").and_then(Value::as_array) {
                for email in list {
                    if let Some(m) = jmap_email_to_message(email, mailboxes, account_email) {
                        out.push(m);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Get the current JMAP Email state string (for the incremental cursor).
fn fetch_email_state(session: &Session, token: &str) -> Result<String> {
    let calls = json!([[
        "Email/get",
        {
            "accountId": session.account_id,
            "ids": [],
            "properties": [],
        },
        "s0"
    ]]);
    let responses = jmap_call(session, token, calls)?;
    if let Some(arr) = responses.as_array() {
        for resp in arr {
            let args = resp.get(1).unwrap_or(&Value::Null);
            if let Some(s) = args.get("state").and_then(Value::as_str) {
                return Ok(s.to_string());
            }
        }
    }
    bail!("JMAP Email/get (ids=[]) returned no state")
}

/// One page of backfill ids from `Email/query`, sorted oldest-first.
///
/// Uses RFC 8620 §5.5 anchor paging: after the first page we pass the last id
/// from the previous page as `anchor` with `anchorOffset: 1`.  This is stable
/// against concurrent deletes — a deleted message shifts numeric positions but
/// does not affect anchor-relative offsets.
///
/// `excluded_ids` is the list of Trash/Junk mailbox ids to exclude via
/// `inMailboxOtherThan`.  An empty list means no exclusion filter (all mailboxes
/// are included), so callers MUST populate this list to honour the Trash/Spam
/// exclusion promise.
///
/// Returns (ids, total) — `ids` may be empty when at the end.
fn fetch_query_page(
    session: &Session,
    token: &str,
    anchor: Option<&str>,
    excluded_ids: &[String],
) -> Result<(Vec<String>, u64)> {
    let mut args = serde_json::Map::new();
    args.insert("accountId".into(), json!(session.account_id));
    // Exclude Trash and Junk by their mailbox ids (RFC 8621 §4.4.2).
    args.insert("filter".into(), json!({
        "inMailboxOtherThan": excluded_ids,
    }));
    args.insert("sort".into(), json!([{"property": "receivedAt", "isAscending": true}]));
    args.insert("limit".into(), json!(PAGE_SIZE));
    args.insert("calculateTotal".into(), json!(true));
    match anchor {
        None => {
            // First page — start from the beginning.
            args.insert("position".into(), json!(0u64));
        }
        Some(a) => {
            // Subsequent pages — anchor to the last id seen + offset 1.
            args.insert("anchor".into(), json!(a));
            args.insert("anchorOffset".into(), json!(1u64));
        }
    }
    let calls = json!([["Email/query", args, "q0"]]);
    let responses = jmap_call(session, token, calls)?;
    if let Some(arr) = responses.as_array() {
        for resp in arr {
            let args = resp.get(1).unwrap_or(&Value::Null);
            let ids: Vec<String> = args
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let total = args.get("total").and_then(Value::as_u64).unwrap_or(0);
            return Ok((ids, total));
        }
    }
    Ok((Vec::new(), 0))
}

/// Incremental: IDs created/updated/destroyed since `state`.
/// Returns (created_ids, new_state, cannot_calculate).
fn fetch_changes(
    session: &Session,
    token: &str,
    state: &str,
) -> Result<(Vec<String>, String, bool)> {
    let calls = json!([[
        "Email/changes",
        {
            "accountId": session.account_id,
            "sinceState": state,
            "maxChanges": 500u64,
        },
        "c0"
    ]]);
    let responses = jmap_call(session, token, calls)?;
    if let Some(arr) = responses.as_array() {
        for resp in arr {
            // Error response: check for cannotCalculateChanges
            if resp.get(0).and_then(Value::as_str) == Some("error") {
                let kind = resp
                    .get(1)
                    .and_then(|a| a.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if kind == "cannotCalculateChanges" {
                    return Ok((Vec::new(), state.to_string(), true));
                }
                let desc = resp
                    .get(1)
                    .and_then(|a| a.get("description"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                bail!("Email/changes error: {desc}");
            }
            let args = resp.get(1).unwrap_or(&Value::Null);
            let created: Vec<String> = args
                .get("created")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let new_state = args
                .get("newState")
                .and_then(Value::as_str)
                .unwrap_or(state)
                .to_string();
            return Ok((created, new_state, false));
        }
    }
    Ok((Vec::new(), state.to_string(), false))
}

// ---------------------------------------------------------------------------
// JMAP Email JSON → correspondence::Message

/// Returns the set of mailbox ids from `mailboxes` whose JMAP `role` matches
/// one of the excluded roles (trash, junk).  Used to build the
/// `inMailboxOtherThan` filter for `Email/query` and to gate the incremental
/// path.
fn excluded_mailbox_ids(mailboxes: &BTreeMap<String, Mailbox>) -> Vec<String> {
    mailboxes
        .iter()
        .filter(|(_, mb)| {
            mb.role.as_deref() == Some("trash") || mb.role.as_deref() == Some("junk")
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// One JMAP Email JSON object → a correspondence::Message. Returns `None` for
/// messages we can't date (effectively invalid), or for messages that live
/// exclusively in excluded (Trash / Spam) mailboxes.
fn jmap_email_to_message(
    email: &Value,
    mailboxes: &BTreeMap<String, Mailbox>,
    account_email: &str,
) -> Option<Message> {
    // `receivedAt` is RFC 3339 — the server-canonical delivery timestamp.
    let received_at = email.get("receivedAt").and_then(Value::as_str)?;
    // Parse to validate + convert to local-time RFC3339 (the rest of the vault uses this).
    let dt = DateTime::parse_from_rfc3339(received_at).ok()?;
    let local_ts = dt.with_timezone(&Local).to_rfc3339();

    let mut m = Message::new("email", local_ts);
    m.service = account_email.to_lowercase();

    // guid = Message-ID header (first entry), or a hash fallback.
    m.guid = email
        .get("messageId")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|id| {
            // Normalise: ensure angle-brackets like other collectors.
            if id.starts_with('<') { id.to_string() } else { format!("<{id}>") }
        })
        .unwrap_or_else(|| {
            // No Message-ID → hash the JMAP id (server-unique, stable).
            let jmap_id = email.get("id").and_then(Value::as_str).unwrap_or("");
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(jmap_id.as_bytes());
            format!("sha256:{:x}", h.finalize())
        });

    // From field.
    if let Some(from_arr) = email.get("from").and_then(Value::as_array) {
        if let Some(addr) = from_arr.first() {
            let address = addr
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase();
            m.from_me = address == m.service;
            m.sender = address;
            m.sender_name = addr
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
    }

    // To + Cc recipients.
    for field in &["to", "cc"] {
        if let Some(arr) = email.get(field).and_then(Value::as_array) {
            for addr in arr {
                if let Some(e) = addr.get("email").and_then(Value::as_str) {
                    m.to.push(e.to_lowercase());
                }
            }
        }
    }

    // Subject → also populates the `chat` thread key (subject stripped of Re:/Fwd:).
    m.subject = email
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    m.chat = strip_thread_prefix(&m.subject);

    // In-Reply-To.
    m.reply_to = email
        .get("inReplyTo")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|id| if id.starts_with('<') { id.to_string() } else { format!("<{id}>") })
        .unwrap_or_default();

    // Body text from `bodyValues` (text parts only).
    if let (Some(text_parts), Some(body_values)) = (
        email.get("textBody").and_then(Value::as_array),
        email.get("bodyValues").and_then(Value::as_object),
    ) {
        let mut text = String::new();
        for part in text_parts {
            if let Some(part_id) = part.get("partId").and_then(Value::as_str) {
                if let Some(bv) = body_values.get(part_id) {
                    if let Some(v) = bv.get("value").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(v.trim());
                    }
                }
            }
        }
        m.text = text;
    }

    // Labels: mailbox names resolved from `mailboxIds`.
    // Also filter out messages that live ONLY in Trash or Junk (excluded roles).
    // This is the incremental-path guard: the backfill query already excludes
    // these mailboxes via `inMailboxOtherThan`, but Email/changes delivers
    // account-wide events so we must post-filter here.
    if let Some(mailbox_ids) = email.get("mailboxIds").and_then(Value::as_object) {
        // Check whether every mailbox this message belongs to is excluded.
        let all_excluded = !mailbox_ids.is_empty()
            && mailbox_ids.keys().all(|id| {
                mailboxes
                    .get(id)
                    .and_then(|mb| mb.role.as_deref())
                    .map(|r| r == "trash" || r == "junk")
                    .unwrap_or(false)
            });
        if all_excluded {
            return None;
        }
        let mut labels: Vec<String> = mailbox_ids
            .keys()
            .filter_map(|id| mailboxes.get(id))
            .map(|mb| mb.name.clone())
            .collect();
        labels.sort();
        m.labels = labels;
    }

    // Keywords → if `$sent` keyword, override from_me (Fastmail's authoritative sent flag).
    if let Some(kws) = email.get("keywords").and_then(Value::as_object) {
        if kws.contains_key("$sent") {
            m.from_me = true;
        }
    }

    Some(m)
}

/// Strip Re:/Fwd: and similar prefixes from a subject to derive the thread key,
/// matching the `mail-parser` `thread_name()` behaviour used by the mbox importer.
fn strip_thread_prefix(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_lowercase();
        let prefix = &["re:", "fwd:", "fw:", "re :", "fwd :", "[re]", "[fwd]"]
            .iter()
            .find(|p| lower.starts_with(*p));
        match prefix {
            Some(p) => s = s[p.len()..].trim(),
            None => break,
        }
    }
    let out = s.to_lowercase();
    if out.is_empty() { "(no subject)".into() } else { out }
}

// ---------------------------------------------------------------------------
// Pull orchestration

/// Load the API token from the secret store. `None` = not connected.
fn load_token(vault: &Vault) -> Result<Option<String>> {
    Ok(vault.load_sync_token(SERVICE)?.map(|t| t.access_token))
}

impl Vault {
    pub(crate) fn read_fastmail_sync(&self) -> FastmailSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_fastmail_sync(&self, state: &FastmailSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

/// Main pull — backfill (if unfinished) then incremental. Returns messages
/// written this pass. Token must exist (caller checks).
fn pull_inner(vault: &Vault) -> Result<u64> {
    let token = load_token(vault)?
        .ok_or_else(|| anyhow::anyhow!("Fastmail is not connected — add your API token in the Integrations tab"))?;

    let session_value = fetch_session(SESSION_URL, &token)
        .context("connecting to Fastmail — check that the API token is still valid")?;
    let session = parse_session(&session_value)?;

    // Update the stored email address if we can read it.
    let mut state = vault.read_fastmail_sync();
    if state.email.is_none() {
        state.email = session_primary_email(&session_value);
    }

    // Fetch mailbox metadata (name + role) once per pass for label resolution
    // and Trash/Spam exclusion.
    let mailboxes = fetch_mailboxes(&session, &token)
        .unwrap_or_default();

    // Build the exclusion list once — Trash (role="trash") and Junk/Spam (role="junk").
    // Passed to Email/query as `inMailboxOtherThan` so those mailboxes are never
    // included in the backfill result set.
    let excluded_ids: Vec<String> = excluded_mailbox_ids(&mailboxes);

    let account_email = state.email.clone().unwrap_or_default();

    // Shared dedupe set (loaded once, grown as we write).
    let mut seen = vault.correspondence_guids("email")?;
    let mut written: u64 = 0;

    // Phase 0: capture the baseline JMAP state before the first backfill page
    // so mail arriving during a long backfill is caught by incremental later.
    if !state.backfill_done && state.jmap_state.is_none() {
        state.jmap_state = Some(fetch_email_state(&session, &token)?);
        // backfill_anchor = None means "start from the top" (no anchor yet).
        vault.write_fastmail_sync(&state)?;
    }

    // Phase A: backfill (page through Email/query oldest-first, anchor-paged).
    if !state.backfill_done {
        // `current_anchor` is None for the first page, Some(last_id) for subsequent pages.
        let mut current_anchor: Option<String> = state.backfill_anchor.clone();
        loop {
            let (ids, _total) =
                fetch_query_page(&session, &token, current_anchor.as_deref(), &excluded_ids)?;
            if ids.is_empty() {
                // Exhausted the mailbox.
                state.backfill_done = true;
                state.backfill_anchor = None;
                vault.write_fastmail_sync(&state)?;
                break;
            }
            let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let msgs = fetch_email_details(&session, &token, &id_refs, &mailboxes, &account_email)?;
            let n = write_new_messages(vault, &msgs, &mut seen)?;
            state.messages += n;
            written += n;
            // Anchor for the next page = last id on this page.
            let last_id = ids.last().cloned();
            current_anchor = last_id.clone();
            state.backfill_anchor = last_id;
            vault.write_fastmail_sync(&state)?;

            if ids.len() < PAGE_SIZE as usize {
                // Last page (short).
                state.backfill_done = true;
                state.backfill_anchor = None;
                vault.write_fastmail_sync(&state)?;
                break;
            }
        }
    }

    // Phase B: incremental from the stored JMAP state.
    // `jmap_email_to_message` already post-filters Trash/Junk for messages
    // delivered via Email/changes (which is account-wide).
    if state.backfill_done {
        if let Some(jmap_state) = state.jmap_state.clone() {
            let (created_ids, new_state, cannot_calculate) =
                fetch_changes(&session, &token, &jmap_state)?;

            if cannot_calculate {
                // Cursor aged out — reset for a fresh backfill (guid dedupe absorbs it).
                state.backfill_done = false;
                state.backfill_anchor = None;
                state.jmap_state = Some(fetch_email_state(&session, &token)?);
                vault.write_fastmail_sync(&state)?;
                // Don't backfill immediately in this pass — pick up next tick.
            } else {
                // Fetch in batches of BATCH_SIZE.
                let mut all_written: u64 = 0;
                for chunk in created_ids.chunks(BATCH_SIZE as usize) {
                    let id_refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
                    let msgs = fetch_email_details(
                        &session,
                        &token,
                        &id_refs,
                        &mailboxes,
                        &account_email,
                    )?;
                    all_written += write_new_messages(vault, &msgs, &mut seen)?;
                }
                state.messages += all_written;
                written += all_written;
                // Only advance the cursor when all created ids were fetched.
                state.jmap_state = Some(new_state);
                vault.write_fastmail_sync(&state)?;
            }
        }
    }

    state.updated = Local::now().to_rfc3339();
    state.error = None;
    vault.write_fastmail_sync(&state)?;
    Ok(written)
}

/// Write messages not already in `seen` to the email stream; grow `seen`.
fn write_new_messages(
    vault: &Vault,
    msgs: &[Message],
    seen: &mut HashSet<String>,
) -> Result<u64> {
    let mut batch: Vec<&Message> = Vec::new();
    for m in msgs {
        if m.guid.is_empty() {
            continue;
        }
        if seen.insert(m.guid.clone()) {
            batch.push(m);
        }
    }
    let n = batch.len() as u64;
    if !batch.is_empty() {
        let owned: Vec<Message> = batch.into_iter().cloned().collect();
        vault.append_messages(&owned)?;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-fastmail-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // strip_thread_prefix

    #[test]
    fn strip_re_fwd_variants() {
        assert_eq!(strip_thread_prefix("Re: Lunch plans"), "lunch plans");
        assert_eq!(strip_thread_prefix("RE: Re: Lunch plans"), "lunch plans");
        assert_eq!(strip_thread_prefix("Fwd: Lunch"), "lunch");
        assert_eq!(strip_thread_prefix("FW: Lunch"), "lunch");
        assert_eq!(strip_thread_prefix("No prefix"), "no prefix");
        assert_eq!(strip_thread_prefix(""), "(no subject)");
    }

    // -----------------------------------------------------------------------
    // jmap_email_to_message — fixture tests from RFC 8621 example shapes

    fn make_mailboxes() -> BTreeMap<String, Mailbox> {
        let mut m = BTreeMap::new();
        m.insert("inbox-id".into(), Mailbox { name: "INBOX".into(), role: Some("inbox".into()) });
        m.insert("sent-id".into(), Mailbox { name: "Sent".into(), role: Some("sent".into()) });
        m.insert("archive-id".into(), Mailbox { name: "Archive".into(), role: None });
        m.insert("trash-id".into(), Mailbox { name: "Trash".into(), role: Some("trash".into()) });
        m.insert("junk-id".into(), Mailbox { name: "Spam".into(), role: Some("junk".into()) });
        m
    }

    /// A typical inbound JMAP Email object.
    const INBOX_EMAIL_JSON: &str = r#"{
        "id": "M01234",
        "receivedAt": "2026-06-10T16:00:00Z",
        "messageId": ["<lunch-invite@example.com>"],
        "inReplyTo": [],
        "from": [{"name": "Alice Example", "email": "alice@example.com"}],
        "to": [{"name": "David", "email": "me@fastmail.com"}],
        "cc": [],
        "subject": "Lunch plans",
        "mailboxIds": {"inbox-id": true},
        "keywords": {"$seen": true},
        "hasAttachment": false,
        "textBody": [{"partId": "p1", "type": "text/plain"}],
        "bodyValues": {"p1": {"value": "Want to grab lunch?", "isTruncated": false}}
    }"#;

    /// A reply with a $sent keyword (Fastmail's authoritative sent flag).
    const SENT_EMAIL_JSON: &str = r#"{
        "id": "M05678",
        "receivedAt": "2026-06-10T16:30:00Z",
        "messageId": ["<my-reply@fastmail.com>"],
        "inReplyTo": ["<lunch-invite@example.com>"],
        "from": [{"name": "David", "email": "me@fastmail.com"}],
        "to": [{"name": "Alice", "email": "alice@example.com"}],
        "cc": [],
        "subject": "Re: Lunch plans",
        "mailboxIds": {"sent-id": true},
        "keywords": {"$sent": true, "$seen": true},
        "hasAttachment": false,
        "textBody": [{"partId": "p2", "type": "text/plain"}],
        "bodyValues": {"p2": {"value": "Noon sounds great!", "isTruncated": false}}
    }"#;

    /// A message without a Message-ID header.
    const NO_MID_EMAIL_JSON: &str = r#"{
        "id": "M99999",
        "receivedAt": "2026-06-11T10:00:00Z",
        "messageId": [],
        "inReplyTo": [],
        "from": [{"name": "Bot", "email": "bot@somewhere.com"}],
        "to": [{"name": "Me", "email": "me@fastmail.com"}],
        "cc": [],
        "subject": "No message id",
        "mailboxIds": {"inbox-id": true},
        "keywords": {},
        "hasAttachment": false,
        "textBody": [{"partId": "p3", "type": "text/plain"}],
        "bodyValues": {"p3": {"value": "Automated notice.", "isTruncated": false}}
    }"#;

    #[test]
    fn parses_inbound_email_fields() {
        let v: Value = serde_json::from_str(INBOX_EMAIL_JSON).unwrap();
        let m = jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com").unwrap();
        assert_eq!(m.guid, "<lunch-invite@example.com>");
        assert_eq!(m.sender, "alice@example.com");
        assert_eq!(m.sender_name, "Alice Example");
        assert!(!m.from_me);
        assert_eq!(m.subject, "Lunch plans");
        assert_eq!(m.chat, "lunch plans");
        assert_eq!(m.to, vec!["me@fastmail.com"]);
        assert_eq!(m.text.trim(), "Want to grab lunch?");
        assert_eq!(m.labels, vec!["INBOX"]);
        assert_eq!(m.service, "me@fastmail.com");
        assert_eq!(m.source, "email");
        // Timestamp: 2026-06-10T16:00:00Z → local (UTC+0 here means same day at 16:00).
        assert!(m.ts.starts_with("2026-06-10"), "ts={}", m.ts);
    }

    #[test]
    fn sent_keyword_overrides_from_me() {
        let v: Value = serde_json::from_str(SENT_EMAIL_JSON).unwrap();
        let m = jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com").unwrap();
        assert!(m.from_me, "$sent keyword must make from_me=true");
        assert_eq!(m.guid, "<my-reply@fastmail.com>");
        assert_eq!(m.reply_to, "<lunch-invite@example.com>");
        assert_eq!(m.chat, "lunch plans", "Re: prefix stripped");
        assert_eq!(m.labels, vec!["Sent"]);
    }

    #[test]
    fn no_message_id_gets_sha256_guid() {
        let v: Value = serde_json::from_str(NO_MID_EMAIL_JSON).unwrap();
        let m = jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com").unwrap();
        assert!(m.guid.starts_with("sha256:"), "guid={}", m.guid);
        // A second call with the same JMAP id gives the same guid (stable).
        let m2 = jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com").unwrap();
        assert_eq!(m.guid, m2.guid, "sha256 guid is deterministic");
    }

    #[test]
    fn undateable_email_returns_none() {
        let v: Value = serde_json::from_str(
            r#"{"id":"Mx","messageId":[],"from":[],"to":[],"cc":[],"subject":"x",
                "mailboxIds":{},"keywords":{},"hasAttachment":false,
                "textBody":[],"bodyValues":{}}"#,
        )
        .unwrap();
        // No `receivedAt` — must return None.
        assert!(jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com").is_none());
    }

    // -----------------------------------------------------------------------
    // Trash / Spam exclusion

    /// An email in Trash only — should be filtered out by jmap_email_to_message.
    const TRASH_EMAIL_JSON: &str = r#"{
        "id": "Mtrash1",
        "receivedAt": "2026-06-10T09:00:00Z",
        "messageId": ["<deleted@example.com>"],
        "inReplyTo": [],
        "from": [{"name": "Spammer", "email": "spam@bad.com"}],
        "to": [{"name": "Me", "email": "me@fastmail.com"}],
        "cc": [],
        "subject": "Deleted email",
        "mailboxIds": {"trash-id": true},
        "keywords": {},
        "hasAttachment": false,
        "textBody": [{"partId": "p9", "type": "text/plain"}],
        "bodyValues": {"p9": {"value": "This was deleted.", "isTruncated": false}}
    }"#;

    /// An email in Junk/Spam only — should be filtered out.
    const JUNK_EMAIL_JSON: &str = r#"{
        "id": "Mjunk1",
        "receivedAt": "2026-06-10T09:30:00Z",
        "messageId": ["<spam@bad.com>"],
        "inReplyTo": [],
        "from": [{"name": "Spammer", "email": "spam@bad.com"}],
        "to": [{"name": "Me", "email": "me@fastmail.com"}],
        "cc": [],
        "subject": "Buy now!!!",
        "mailboxIds": {"junk-id": true},
        "keywords": {"$junk": true},
        "hasAttachment": false,
        "textBody": [{"partId": "pa", "type": "text/plain"}],
        "bodyValues": {"pa": {"value": "Click here!", "isTruncated": false}}
    }"#;

    /// An email in both INBOX and Trash (e.g. moved but not yet purged) — should
    /// NOT be filtered because it still lives in a non-excluded mailbox.
    const INBOX_AND_TRASH_EMAIL_JSON: &str = r#"{
        "id": "Mmulti1",
        "receivedAt": "2026-06-10T10:00:00Z",
        "messageId": ["<multi@example.com>"],
        "inReplyTo": [],
        "from": [{"name": "Alice", "email": "alice@example.com"}],
        "to": [{"name": "Me", "email": "me@fastmail.com"}],
        "cc": [],
        "subject": "Partly moved",
        "mailboxIds": {"inbox-id": true, "trash-id": true},
        "keywords": {},
        "hasAttachment": false,
        "textBody": [],
        "bodyValues": {}
    }"#;

    #[test]
    fn trash_only_email_is_excluded() {
        let v: Value = serde_json::from_str(TRASH_EMAIL_JSON).unwrap();
        assert!(
            jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com").is_none(),
            "email in Trash-only should be filtered out"
        );
    }

    #[test]
    fn junk_only_email_is_excluded() {
        let v: Value = serde_json::from_str(JUNK_EMAIL_JSON).unwrap();
        assert!(
            jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com").is_none(),
            "email in Junk-only should be filtered out"
        );
    }

    #[test]
    fn email_in_inbox_and_trash_is_not_excluded() {
        let v: Value = serde_json::from_str(INBOX_AND_TRASH_EMAIL_JSON).unwrap();
        let m = jmap_email_to_message(&v, &make_mailboxes(), "me@fastmail.com");
        assert!(m.is_some(), "email in both INBOX and Trash should NOT be filtered");
    }

    #[test]
    fn excluded_mailbox_ids_returns_trash_and_junk() {
        let mbs = make_mailboxes();
        let mut ids = excluded_mailbox_ids(&mbs);
        ids.sort();
        assert_eq!(ids, vec!["junk-id", "trash-id"]);
    }

    #[test]
    fn sync_state_back_compat_old_position_field() {
        // Old JSON with `backfill_position` field (numeric cursor) should
        // deserialise cleanly — the field is unknown to the new struct so serde
        // skips it, and `backfill_anchor` defaults to None.
        let old_json = r#"{
            "email": "u@fastmail.com",
            "jmap_state": "s99",
            "backfill_done": false,
            "backfill_position": 150,
            "messages": 150
        }"#;
        let s: FastmailSyncState = serde_json::from_str(old_json).unwrap();
        assert_eq!(s.email.as_deref(), Some("u@fastmail.com"));
        assert!(!s.backfill_done);
        // Old position field ignored; new anchor defaults to None (backfill restarts,
        // guid dedupe absorbs the overlap).
        assert!(s.backfill_anchor.is_none(), "old position field must not populate anchor");
    }

    // -----------------------------------------------------------------------
    // parse_session

    /// A minimal JMAP session document (from RFC 8620 §2).
    const SESSION_JSON: &str = r#"{
        "capabilities": {
            "urn:ietf:params:jmap:core": {},
            "urn:ietf:params:jmap:mail": {}
        },
        "accounts": {
            "A123": {
                "name": "user@fastmail.com",
                "isPersonal": true,
                "isReadOnly": false,
                "accountCapabilities": {}
            }
        },
        "primaryAccounts": {
            "urn:ietf:params:jmap:core": "A123",
            "urn:ietf:params:jmap:mail": "A123"
        },
        "username": "user@fastmail.com",
        "apiUrl": "https://api.fastmail.com/jmap/api/",
        "downloadUrl": "https://api.fastmail.com/jmap/download/{accountId}/{blobId}/{name}?accept={type}",
        "uploadUrl": "https://api.fastmail.com/jmap/upload/{accountId}/",
        "eventSourceUrl": "https://api.fastmail.com/jmap/eventsource/",
        "state": "s1"
    }"#;

    #[test]
    fn parse_session_extracts_account_id_and_urls() {
        let v: Value = serde_json::from_str(SESSION_JSON).unwrap();
        let s = parse_session(&v).unwrap();
        assert_eq!(s.account_id, "A123");
        assert_eq!(s.api_url, "https://api.fastmail.com/jmap/api/");
    }

    #[test]
    fn session_primary_email_extracts_name() {
        let v: Value = serde_json::from_str(SESSION_JSON).unwrap();
        assert_eq!(
            session_primary_email(&v).as_deref(),
            Some("user@fastmail.com")
        );
    }

    // -----------------------------------------------------------------------
    // Cursor / sync state round-trip

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        let state = FastmailSyncState {
            email: Some("user@fastmail.com".into()),
            jmap_state: Some("s42".into()),
            backfill_done: true,
            backfill_anchor: None,
            messages: 1234,
            updated: "2026-06-10T10:00:00-07:00".into(),
            error: None,
        };
        v.write_fastmail_sync(&state).unwrap();
        let loaded = v.read_fastmail_sync();
        assert_eq!(loaded.email.as_deref(), Some("user@fastmail.com"));
        assert_eq!(loaded.jmap_state.as_deref(), Some("s42"));
        assert!(loaded.backfill_done);
        assert_eq!(loaded.messages, 1234);
    }

    #[test]
    fn sync_state_defaults_when_missing() {
        let v = temp_vault("defaults");
        let s = v.read_fastmail_sync();
        assert!(s.email.is_none());
        assert!(s.jmap_state.is_none());
        assert!(!s.backfill_done);
        assert_eq!(s.messages, 0);
    }

    // -----------------------------------------------------------------------
    // write_new_messages — deduplication

    fn make_msg(guid: &str, ts: &str) -> Message {
        Message {
            guid: guid.into(),
            ..Message::new("email", ts.into())
        }
    }

    #[test]
    fn write_new_dedupes_and_appends_to_email_stream() {
        let v = temp_vault("dedup");
        let mut seen = v.correspondence_guids("email").unwrap();

        let msgs = vec![
            make_msg("<a@x>", "2026-06-10T09:00:00-07:00"),
            make_msg("<b@x>", "2026-06-10T10:00:00-07:00"),
        ];
        let n = write_new_messages(&v, &msgs, &mut seen).unwrap();
        assert_eq!(n, 2);

        // Re-write: both guids are now in `seen` — none written again.
        let n2 = write_new_messages(&v, &msgs, &mut seen).unwrap();
        assert_eq!(n2, 0);

        // The stream holds exactly 2 messages.
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
    }

    #[test]
    fn messages_land_in_email_source_and_service_is_account() {
        let v = temp_vault("stream");
        let mut seen = v.correspondence_guids("email").unwrap();
        let mut m = make_msg("<c@x>", "2026-06-11T08:00:00-07:00");
        m.service = "me@fastmail.com".into();
        write_new_messages(&v, &[m], &mut seen).unwrap();
        let day = v.correspondence_timeline("2026-06-11").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "email");
        assert_eq!(day[0].service, "me@fastmail.com");
    }

    // -----------------------------------------------------------------------
    // Connection / status (no live network)

    #[test]
    fn status_empty_when_not_connected() {
        let v = temp_vault("status-empty");
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
    }

    #[test]
    fn status_populated_after_token_stored() {
        let v = temp_vault("status-connected");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "mytoken123".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let mut state = v.read_fastmail_sync();
        state.email = Some("me@fastmail.com".into());
        v.write_fastmail_sync(&state).unwrap();

        let s = def_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert_eq!(s.accounts[0].label, "me@fastmail.com");
        assert!(!s.accounts[0].needs_reconnect);
    }

    #[test]
    fn disconnect_removes_token() {
        let v = temp_vault("disconnect");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "tok".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        assert!(v.load_sync_token(SERVICE).unwrap().is_some());
        def_disconnect(&v, SERVICE).unwrap();
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
        // Status is now empty.
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn connection_exposes_token_paste() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "fastmail");
    }
}
