//! Apple Mail (local Envelope Index) — reads every message from Mail.app's
//! SQLite Envelope Index without credentials: no OAuth, no token, just Full
//! Disk Access (already on Trove's permission ladder for iMessage).
//!
//! Writes the **unified email sink** (`correspondence/email/YYYY-MM.jsonl`,
//! see [`crate::correspondence`]) using source `"email"`, identical to the
//! mbox importer ([`crate::email`]) and the Gmail puller ([`crate::gmail`]).
//! The shared `guid` = RFC 5322 Message-ID from `message_global_data` means
//! all three paths deduplicate the same message automatically.
//!
//! # Envelope Index layout (V9 / V10, macOS 12-26)
//!
//! The database lives at `~/Library/Mail/V10/MailData/Envelope Index`
//! (V9 on Monterey: `…/V9/…`). Relevant tables:
//!
//! - `messages` — one row per message; ROWID is the monotonic cursor key;
//!   `date_sent` and `date_received` are plain Unix seconds (not Apple epoch);
//!   `sender` FK → `addresses.ROWID`; `subject` FK → `subjects.ROWID`;
//!   `summary` FK → `summaries.ROWID`; `mailbox` FK → `mailboxes.ROWID`.
//! - `addresses` — (`address` TEXT, `comment` TEXT) — plain email + display
//!   name.
//! - `subjects` — (`subject` TEXT).
//! - `summaries` — (`summary` TEXT) — Mail.app's body snippet or cached
//!   preview text (truncated for most messages; full body requires .emlx,
//!   deferred to v2).  May be absent for very old messages or cloud-only
//!   messages not yet downloaded; we fall back to empty text so the row is
//!   still stored.
//! - `message_global_data` — (`message_id` INTEGER PK, `message_id_header`
//!   TEXT) — the RFC 5322 Message-ID header as a string, e.g.
//!   `<abc@example.com>`.  The `message_id` column matches `messages.message_id`
//!   (NOT `messages.ROWID`).
//! - `recipients` — (`message` FK→messages.ROWID, `address` FK→addresses.ROWID,
//!   `type` INTEGER) — to (0) and cc (1) recipients; we collect both.
//! - `attachments` — (`message` FK→messages.ROWID, `name` TEXT) — attachment
//!   filename (no MIME or size in this table; full metadata in .emlx only).
//! - `mailboxes` — (`url` TEXT) — IMAP URL whose path component encodes both
//!   the account UUID and the folder name; we use it to detect sent folders.
//!
//! # from_me detection
//!
//! We collect all sender addresses that appear in `Sent`-named mailboxes on
//! the first pass, building a local-address set. Every message whose sender
//! address is in that set has `from_me = true`.  This survives multi-account
//! setups and is rebuild-safe.
//!
//! # service field (account address)
//!
//! `service` is the **owner account's** email address for each message — the
//! same semantics as `email.rs` (passes `account` param) and `gmail.rs` (uses
//! `account_email`). It is NOT the sender address.  We derive it from the
//! mailbox URL: `imap://<UUID>/INBOX` → UUID → owner address (looked up from
//! Sent-folder senders for that UUID).  This gives the correct per-account
//! chip counts in the email browser.
//!
//! # Duplicate-message dedup
//!
//! The same RFC 5322 message can appear in multiple Apple Mail mailboxes
//! (INBOX, [Gmail]/All Mail, labels, etc.) as distinct `messages` rows that
//! share `messages.message_id` (Apple's internal cross-mailbox hash, not
//! `ROWID`).  We dedup within each pass by tracking seen `message_id` values
//! (preferring INBOX / minimum ROWID), so only one record per logical email
//! reaches the vault.  The ROWID cursor still advances to `MAX(ROWID)` over
//! all scanned rows so incremental passes stay correct.
//!
//! # Sync cursor
//!
//! ROWID watermark persisted in `.trove/apple-mail-sync.json`, identical in
//! structure to the iMessage cursor.  A missing sync file causes a full
//! backfill; guid dedupe absorbs any overlap.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::import_via_copy;
use crate::correspondence::{AttachmentMeta, Message};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

/// Seconds between Apple Mail syncs (same cadence as iMessage).
pub const APPLE_MAIL_SYNC_SECS: u64 = 900;

/// Non-secret cursor file — not under `.trove/sync/` (which is 0600 secrets).
const SYNC_FILE: &str = ".trove/apple-mail-sync.json";

/// Source tag written to every record — shared with email.rs / gmail.rs so
/// the three deduplicate on Message-ID.
const SOURCE: &str = "email";

// ---------------------------------------------------------------------------
// DEF

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_apple_mail()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_messages > 0, || {
        format!("imported {} messages", s.new_messages)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(apple_mail_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_apple_mail_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-mail",
        name: "Apple Mail",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Syncs every email account configured in Mail.app from the local \
                      Envelope Index database — no credentials required. Writes the same \
                      unified email stream as the Gmail pull and .mbox import, \
                      deduplicated on Message-ID.",
        domain: "correspondence",
        vault_path: "correspondence/email/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
            "Apple Mail must be configured and have synced at least once before messages appear.",
        ],
        caveats: "Covers only messages Mail.app has downloaded to this Mac. \
                  Body text comes from Mail's local cache (the summaries table); messages \
                  not yet synced will have empty text. Full .emlx bodies are not parsed \
                  in v1 — the summary cache is used instead.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(APPLE_MAIL_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Path resolution (V9 / V10)

/// Returns the first `Envelope Index` path that exists, probing V10 before V9.
pub fn envelope_index_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    for ver in ["V10", "V9"] {
        let p = home.join(format!("Library/Mail/{ver}/MailData/Envelope Index"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Whether this process can read the Envelope Index.
pub fn apple_mail_permission_ok() -> bool {
    envelope_index_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Sync state

/// Incremental sync state, persisted in `.trove/apple-mail-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AppleMailSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest imported `messages.ROWID`.
    pub cursor: i64,
}

/// Result of one sync pass.
#[derive(Debug, Clone, Serialize)]
pub struct AppleMailSyncStats {
    /// False while the Envelope Index is unreadable (no Full Disk Access).
    pub available: bool,
    pub new_messages: u64,
}

// ---------------------------------------------------------------------------
// Core import logic (split from copy step so tests can use synthetic DBs)

/// Account info derived from Sent-folder messages: the set of owner addresses
/// (for `from_me` detection) plus a mailbox-ROWID → owner-address map (for
/// `service`).
struct AccountInfo {
    /// Lowercase email addresses that belong to the vault owner (from any Sent
    /// folder across all accounts).
    local_addresses: HashSet<String>,
    /// `mailboxes.ROWID` → lowercase owner address for that mailbox's account.
    ///
    /// Derived by extracting the UUID from the IMAP URL (`imap://<UUID>/…`)
    /// and joining back to the Sent-folder senders for that UUID.  Every
    /// mailbox that shares a UUID with a Sent folder maps to the same owner.
    mailbox_account: HashMap<i64, String>,
}

/// Parse the UUID segment out of an IMAP URL like `imap://UUID/INBOX`.
/// Returns `None` when the URL is absent or has no authority component.
fn uuid_from_imap_url(url: &str) -> Option<&str> {
    // Strip scheme: "imap://" or "imaps://"
    let after_scheme = url.strip_prefix("imap://").or_else(|| url.strip_prefix("imaps://"))?;
    // The authority ends at the first '/'
    let uuid = after_scheme.split('/').next()?;
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// Build account info by scanning Sent-named mailboxes:
///
/// 1. Collect `(mailbox_url, sender_address)` for all messages in Sent folders.
/// 2. Extract the UUID from each URL → UUID→sender_address mapping.
/// 3. Walk *all* mailboxes and assign each one the sender address for its UUID.
fn collect_account_info(conn: &rusqlite::Connection) -> AccountInfo {
    // Step 1+2: UUID → owner email from Sent-folder senders.
    let mut uuid_to_owner: HashMap<String, String> = HashMap::new();
    let sql_sent = "
        SELECT mb.url, a.address
          FROM messages m
          JOIN addresses a ON a.ROWID = m.sender
          JOIN mailboxes mb ON mb.ROWID = m.mailbox
         WHERE mb.url LIKE '%Sent%'
           AND a.address != ''
    ";
    if let Ok(mut stmt) = conn.prepare(sql_sent) {
        let _ = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map(|rows| {
                for r in rows.flatten() {
                    let (url, addr) = r;
                    if let Some(uuid) = uuid_from_imap_url(&url) {
                        uuid_to_owner
                            .entry(uuid.to_string())
                            .or_insert_with(|| addr.to_lowercase());
                    }
                }
            });
    }

    // Step 3: Map every mailbox.ROWID to its account owner address.
    let mut mailbox_account: HashMap<i64, String> = HashMap::new();
    let sql_mb = "SELECT ROWID, url FROM mailboxes";
    if let Ok(mut stmt) = conn.prepare(sql_mb) {
        let _ = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map(|rows| {
                for r in rows.flatten() {
                    let (rowid, url) = r;
                    if let Some(uuid) = uuid_from_imap_url(&url) {
                        if let Some(owner) = uuid_to_owner.get(uuid) {
                            mailbox_account.insert(rowid, owner.clone());
                        }
                    }
                }
            });
    }

    let local_addresses: HashSet<String> = uuid_to_owner.values().cloned().collect();
    AccountInfo { local_addresses, mailbox_account }
}

/// Read messages with `messages.ROWID > cursor` out of a copy of the
/// Envelope Index and append them to the vault.  Returns `(rows_imported,
/// new_cursor)`.
pub(crate) fn import_apple_mail_db(
    vault: &Vault,
    db: &Path,
    cursor: i64,
) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening Envelope Index copy {}", db.display()))?;

    let account_info = collect_account_info(&conn);

    // Pre-fetch recipients for all messages we're about to process.
    // recipients.type: 0 = To, 1 = Cc (we collect both).
    let mut recipient_map: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let sql = "
            SELECT r.message, a.address
              FROM recipients r
              JOIN addresses a ON a.ROWID = r.address
             WHERE r.message > ?1
               AND (r.type = 0 OR r.type = 1)
               AND a.address != ''
        ";
        if let Ok(mut stmt) = conn.prepare(sql) {
            let _ = stmt
                .query_map([cursor], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map(|rows| {
                    for r in rows.flatten() {
                        recipient_map.entry(r.0).or_default().push(r.1.to_lowercase());
                    }
                });
        }
    }

    // Pre-fetch attachment names (only name is available in the Envelope
    // Index; full MIME/size are in .emlx).
    let mut attachment_map: HashMap<i64, Vec<AttachmentMeta>> = HashMap::new();
    {
        let sql = "
            SELECT att.message, COALESCE(att.name, '')
              FROM attachments att
             WHERE att.message > ?1
               AND att.name IS NOT NULL
               AND att.name != ''
        ";
        if let Ok(mut stmt) = conn.prepare(sql) {
            let _ = stmt
                .query_map([cursor], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map(|rows| {
                    for r in rows.flatten() {
                        attachment_map.entry(r.0).or_default().push(AttachmentMeta {
                            name: r.1,
                            mime: String::new(),
                            bytes: 0,
                        });
                    }
                });
        }
    }

    // Main query: join messages → sender → subject → summary → mailbox
    // → message_id_header.
    //
    // We fetch `m.message_id` (Apple's cross-mailbox dedup integer) and
    // `m.mailbox` (FK → mailboxes.ROWID, for service derivation) in addition
    // to the payload columns.
    //
    // The join to message_global_data uses messages.message_id (not ROWID).
    // LEFT JOINs everywhere: a missing row produces NULLs, not a dropped
    // message (we skip rows with no usable timestamp only).
    let sql = "
        SELECT m.ROWID,
               COALESCE(gd.message_id_header, ''),
               COALESCE(a.address, ''),
               COALESCE(a.comment, ''),
               COALESCE(sub.subject, ''),
               COALESCE(sum.summary, ''),
               m.date_sent,
               m.message_id,
               m.mailbox
          FROM messages m
          LEFT JOIN message_global_data gd ON gd.message_id = m.message_id
          LEFT JOIN addresses a              ON a.ROWID = m.sender
          LEFT JOIN subjects sub             ON sub.ROWID = m.subject
          LEFT JOIN summaries sum            ON sum.ROWID = m.summary
         WHERE m.ROWID > ?1
           AND m.deleted = 0
         ORDER BY m.ROWID
    ";

    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([cursor])?;

    let mut messages: Vec<Message> = Vec::new();
    // max tracks MAX(ROWID) over ALL scanned rows (including dupe copies) so
    // the cursor always advances past everything we looked at this pass.
    let mut max = cursor;
    // Dedup set: Apple `messages.message_id` is the cross-mailbox stable key.
    // Zero means "no assigned id" (rare, e.g. local drafts never sent); those
    // rows pass through without dedup since they're genuinely distinct.
    let mut seen_apple_ids: HashSet<i64> = HashSet::new();

    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        max = max.max(rowid);

        let msg_id_header: String = row.get(1)?;
        let sender_addr: String = row.get(2)?;
        let sender_name: String = row.get(3)?;
        let subject: String = row.get(4)?;
        let summary: String = row.get(5)?;
        let date_sent: i64 = row.get(6)?;
        let apple_msg_id: i64 = row.get(7)?;
        let mailbox_rowid: i64 = row.get(8)?;

        // Cross-mailbox dedup: skip subsequent copies (higher ROWID) of the
        // same logical email.  apple_msg_id == 0 means no stable id; allow
        // through rather than conflating all such rows.
        if apple_msg_id != 0 {
            if !seen_apple_ids.insert(apple_msg_id) {
                continue; // already stored a row for this message
            }
        }

        // date_sent is plain Unix seconds (not Apple Core Data epoch).
        let Some(local) = DateTime::from_timestamp(date_sent, 0) else {
            continue;
        };
        let local: DateTime<Local> = local.with_timezone(&Local);

        // Build the guid: prefer RFC 5322 Message-ID; fall back to a
        // stable ROWID-based placeholder so we still store the row.
        let guid = if !msg_id_header.is_empty() {
            msg_id_header.clone()
        } else {
            format!("apple-mail-rowid:{rowid}")
        };

        let sender_lc = sender_addr.to_lowercase();
        let from_me = !sender_lc.is_empty() && account_info.local_addresses.contains(&sender_lc);

        // Thread key: use subject (lowercased, Re:/Fwd: stripped) as a
        // cheap thread name consistent with email.rs.
        let base = strip_reply_prefix(&subject);
        let chat = if base.is_empty() { "(no subject)".to_string() } else { base.to_lowercase() };

        // service = the ACCOUNT OWNER's address for this mailbox, not the
        // sender.  This matches email.rs (`account` param) and gmail.rs so
        // the email-browser account chips show the correct ~5 account
        // addresses rather than thousands of distinct sender addresses.
        let service = account_info
            .mailbox_account
            .get(&mailbox_rowid)
            .cloned()
            .unwrap_or_default();

        let mut m = Message::new(SOURCE, local.to_rfc3339());
        m.guid = guid;
        m.rowid = rowid;
        m.sender = sender_lc;
        m.sender_name = sender_name.trim().to_string();
        m.from_me = from_me;
        m.subject = subject;
        m.chat = chat;
        m.text = summary.trim().to_string();
        m.service = service;
        m.to = recipient_map.remove(&rowid).unwrap_or_default();
        m.attachments = attachment_map.remove(&rowid).unwrap_or_default();

        messages.push(m);
    }

    vault.append_messages(&messages)?;
    Ok((messages.len() as u64, max))
}

/// Strip one or more `Re:`, `Fwd:`, `[list-tag]` prefixes from a subject,
/// returning the base thread name.  Pure string scan — cheap and reliable.
fn strip_reply_prefix(subject: &str) -> &str {
    let mut s = subject.trim();
    loop {
        // Skip bracket tags like "[LISTNAME] "
        if s.starts_with('[') {
            if let Some(end) = s.find(']') {
                s = s[end + 1..].trim();
                continue;
            }
        }
        // Skip Re: / RE: / Fwd: / FW: / AW: etc.
        let lower_start: String = s.chars().take(5).collect::<String>().to_lowercase();
        if lower_start.starts_with("re:") {
            s = s[3..].trim();
            continue;
        }
        if lower_start.starts_with("fw:") || lower_start.starts_with("aw:") {
            s = s[3..].trim();
            continue;
        }
        if lower_start.starts_with("fwd:") {
            s = s[4..].trim();
            continue;
        }
        break;
    }
    s
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One incremental sync pass over the Apple Mail Envelope Index. Silently
    /// a no-op (with `available:false`) while the database is unreadable.
    pub fn collect_apple_mail(&self) -> Result<AppleMailSyncStats> {
        if !apple_mail_permission_ok() {
            return Ok(AppleMailSyncStats { available: false, new_messages: 0 });
        }
        let db = envelope_index_path().expect("permission_ok implies path");
        let mut state = self.read_apple_mail_sync().unwrap_or_default();
        let stem = format!("trove-apple-mail-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| {
            import_apple_mail_db(self, tmp, state.cursor)
        })?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_apple_mail_sync(&state)?;
        Ok(AppleMailSyncStats { available: true, new_messages: n })
    }

    /// Read the persisted sync state, if a sync has ever run.
    pub fn read_apple_mail_sync(&self) -> Option<AppleMailSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_apple_mail_sync(&self, state: &AppleMailSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-apple-mail-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Create a minimal fake Envelope Index with the real table schema
    /// observed from a live V10 database.
    fn fake_envelope_index(tag: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir()
            .join(format!("trove-fake-ei-{}-{tag}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                ROWID INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL DEFAULT 0,
                sender INTEGER DEFAULT 0,
                subject INTEGER DEFAULT 0,
                summary INTEGER DEFAULT 0,
                date_sent INTEGER DEFAULT 0,
                mailbox INTEGER DEFAULT 0,
                deleted INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE addresses (
                ROWID INTEGER PRIMARY KEY,
                address TEXT NOT NULL,
                comment TEXT NOT NULL
            );
            CREATE TABLE subjects (
                ROWID INTEGER PRIMARY KEY,
                subject TEXT NOT NULL
            );
            CREATE TABLE summaries (
                ROWID INTEGER PRIMARY KEY,
                summary TEXT NOT NULL
            );
            CREATE TABLE mailboxes (
                ROWID INTEGER PRIMARY KEY,
                url TEXT NOT NULL
            );
            CREATE TABLE message_global_data (
                message_id INTEGER PRIMARY KEY,
                message_id_header TEXT
            );
            CREATE TABLE recipients (
                ROWID INTEGER PRIMARY KEY,
                message INTEGER NOT NULL,
                address INTEGER NOT NULL,
                type INTEGER DEFAULT 0
            );
            CREATE TABLE attachments (
                ROWID INTEGER PRIMARY KEY,
                message INTEGER NOT NULL,
                attachment_id TEXT,
                name TEXT
            );",
        )
        .unwrap();
        (path, conn)
    }

    /// Seed a basic fake Envelope Index with two messages (one incoming,
    /// one sent) plus a reply and an attachment.
    fn seed_basic(conn: &rusqlite::Connection) {
        // Addresses: alice (sender of incoming), david (account owner)
        conn.execute_batch(
            "INSERT INTO addresses VALUES (1, 'alice@example.com', 'Alice Example');
             INSERT INTO addresses VALUES (2, 'david@wills.dev', 'David Wills');
             INSERT INTO addresses VALUES (3, 'bob@example.com', 'Bob Test');",
        )
        .unwrap();

        // Subjects
        conn.execute_batch(
            "INSERT INTO subjects VALUES (1, 'Lunch plans');
             INSERT INTO subjects VALUES (2, 'Re: Lunch plans');",
        )
        .unwrap();

        // Summaries (body snippets / cached text)
        conn.execute_batch(
            "INSERT INTO summaries VALUES (1, 'Want to grab lunch?');
             INSERT INTO summaries VALUES (2, 'Absolutely. Noon?');",
        )
        .unwrap();

        // Mailboxes: Inbox + Sent Messages
        conn.execute_batch(
            "INSERT INTO mailboxes VALUES (1, 'imap://UUID-AAA/INBOX');
             INSERT INTO mailboxes VALUES (2, 'imap://UUID-AAA/Sent%20Messages');",
        )
        .unwrap();

        // message_global_data — message_id joins via messages.message_id
        conn.execute_batch(
            "INSERT INTO message_global_data VALUES (100, '<one@example.com>');
             INSERT INTO message_global_data VALUES (101, '<two@example.com>');",
        )
        .unwrap();

        // Messages: incoming (ROWID 1, mailbox=INBOX) and sent (ROWID 2, mailbox=Sent)
        // date_sent = 1749556800 ≈ 2025-06-10 12:00:00 UTC (noon UTC → same local day
        // regardless of US/EU timezone offset)
        conn.execute_batch(
            "INSERT INTO messages VALUES (1, 100, 1, 1, 1, 1749556800, 1, 0);
             INSERT INTO messages VALUES (2, 101, 2, 2, 2, 1749556860, 2, 0);",
        )
        .unwrap();

        // Recipients: message 1 → to david; message 2 → to alice (To) + bob (Cc)
        conn.execute_batch(
            "INSERT INTO recipients VALUES (1, 1, 2, 0);
             INSERT INTO recipients VALUES (2, 2, 1, 0);
             INSERT INTO recipients VALUES (3, 2, 3, 1);",
        )
        .unwrap();

        // Attachment on the sent message
        conn.execute_batch(
            "INSERT INTO attachments VALUES (1, 2, '1.pdf', 'report.pdf');",
        )
        .unwrap();
    }

    #[test]
    fn basic_import_parses_headers_and_detects_from_me() {
        let v = temp_vault("basic");
        let (db, conn) = fake_envelope_index("basic");
        seed_basic(&conn);
        drop(conn);

        let (n, cursor) = import_apple_mail_db(&v, &db, 0).unwrap();
        assert_eq!(n, 2, "should import both messages");
        assert_eq!(cursor, 2);

        let day = v.correspondence_timeline("2025-06-10").unwrap();
        assert_eq!(day.len(), 2);

        let incoming = &day[0];
        assert_eq!(incoming.guid, "<one@example.com>");
        assert_eq!(incoming.sender, "alice@example.com");
        assert_eq!(incoming.sender_name, "Alice Example");
        assert!(!incoming.from_me);
        assert_eq!(incoming.subject, "Lunch plans");
        assert_eq!(incoming.chat, "lunch plans");
        assert_eq!(incoming.text, "Want to grab lunch?");
        assert_eq!(incoming.to, vec!["david@wills.dev"]);
        // service = owner account address (david@wills.dev from Sent folder),
        // NOT the sender (alice@example.com).
        assert_eq!(incoming.service, "david@wills.dev",
            "service must be the account owner address, not the sender");

        let sent = &day[1];
        assert_eq!(sent.guid, "<two@example.com>");
        assert_eq!(sent.sender, "david@wills.dev");
        assert!(sent.from_me, "sender in a Sent mailbox → from_me = true");
        assert_eq!(sent.chat, "lunch plans", "Re: stripped for thread key");
        assert_eq!(sent.text, "Absolutely. Noon?");
        // Both To and Cc recipients included
        assert!(sent.to.contains(&"alice@example.com".to_string()));
        assert!(sent.to.contains(&"bob@example.com".to_string()));
        // Attachment metadata preserved
        assert_eq!(sent.attachments.len(), 1);
        assert_eq!(sent.attachments[0].name, "report.pdf");
        // service = same account (both mailboxes have UUID-AAA)
        assert_eq!(sent.service, "david@wills.dev",
            "sent message service must be the owner account address");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn incremental_cursor_skips_already_imported() {
        let v = temp_vault("incr");
        let (db, conn) = fake_envelope_index("incr");
        seed_basic(&conn);
        drop(conn);

        let (n1, cursor1) = import_apple_mail_db(&v, &db, 0).unwrap();
        assert_eq!(n1, 2);

        // Re-run with the cursor: nothing new.
        let (n2, cursor2) = import_apple_mail_db(&v, &db, cursor1).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(cursor2, cursor1);

        // Exact 2 records in the vault — no duplicates.
        let day = v.correspondence_timeline("2025-06-10").unwrap();
        assert_eq!(day.len(), 2);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn deleted_messages_are_excluded() {
        let v = temp_vault("deleted");
        let (db, conn) = fake_envelope_index("deleted");
        conn.execute_batch(
            "INSERT INTO addresses VALUES (1, 'x@example.com', 'X');
             INSERT INTO subjects VALUES (1, 'Hello');
             INSERT INTO summaries VALUES (1, 'Hi there');
             INSERT INTO mailboxes VALUES (1, 'imap://U/INBOX');
             -- message_id 1: deleted=1 — must be excluded
             INSERT INTO message_global_data VALUES (1, '<hello@x.com>');
             INSERT INTO messages VALUES (1, 1, 1, 1, 1, 1749556800, 1, 1);
             -- message_id 2: deleted=0 — must be included
             INSERT INTO message_global_data VALUES (2, '<hello2@x.com>');
             INSERT INTO messages VALUES (2, 2, 1, 1, 1, 1749556860, 1, 0);",
        )
        .unwrap();
        drop(conn);

        let (n, _) = import_apple_mail_db(&v, &db, 0).unwrap();
        assert_eq!(n, 1, "deleted message must be excluded");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn message_without_message_id_gets_rowid_guid() {
        let v = temp_vault("noguid");
        let (db, conn) = fake_envelope_index("noguid");
        conn.execute_batch(
            "INSERT INTO addresses VALUES (1, 'x@example.com', 'X');
             INSERT INTO subjects VALUES (1, 'Sub');
             INSERT INTO summaries VALUES (1, 'Body');
             INSERT INTO mailboxes VALUES (1, 'imap://U/INBOX');
             -- No message_global_data row → no Message-ID header
             INSERT INTO messages VALUES (1, 999, 1, 1, 1, 1749556800, 1, 0);",
        )
        .unwrap();
        drop(conn);

        let (n, _) = import_apple_mail_db(&v, &db, 0).unwrap();
        assert_eq!(n, 1);
        let day = v.correspondence_timeline("2025-06-10").unwrap();
        assert!(day[0].guid.starts_with("apple-mail-rowid:"), "fallback guid used");

        let _ = fs::remove_file(db);
    }

    /// Verify that a message appearing in two mailboxes (INBOX and All Mail)
    /// with the same `messages.message_id` is stored exactly once, and that
    /// the ROWID cursor still advances to the maximum seen ROWID.
    #[test]
    fn duplicate_mailbox_copies_deduplicated() {
        let v = temp_vault("dedup");
        let (db, conn) = fake_envelope_index("dedup");
        conn.execute_batch(
            "INSERT INTO addresses VALUES (1, 'alice@example.com', 'Alice');
             INSERT INTO addresses VALUES (2, 'david@wills.dev', 'David');
             INSERT INTO subjects VALUES (1, 'Hello');
             INSERT INTO summaries VALUES (1, 'Hi there');
             -- Two mailboxes: INBOX (ROWID 1) and All Mail (ROWID 2).
             INSERT INTO mailboxes VALUES (1, 'imap://UUID-BBB/INBOX');
             INSERT INTO mailboxes VALUES (2, 'imap://UUID-BBB/[Gmail]/All%20Mail');
             -- Sent folder so UUID-BBB maps to david@wills.dev for service.
             INSERT INTO mailboxes VALUES (3, 'imap://UUID-BBB/Sent%20Messages');
             INSERT INTO message_global_data VALUES (42, '<hello@msg.com>');
             -- Same apple message_id=42 appears in two mailboxes (ROWID 1 and 2).
             INSERT INTO messages VALUES (1, 42, 1, 1, 1, 1749556800, 1, 0);
             INSERT INTO messages VALUES (2, 42, 1, 1, 1, 1749556800, 2, 0);
             -- A message from david to bootstrap the Sent-folder UUID mapping.
             INSERT INTO message_global_data VALUES (43, '<sent@msg.com>');
             INSERT INTO messages VALUES (3, 43, 2, 1, 1, 1749556860, 3, 0);",
        )
        .unwrap();
        drop(conn);

        let (n, max_cursor) = import_apple_mail_db(&v, &db, 0).unwrap();
        // Only 2 distinct messages (one deduped + one sent), not 3 rows.
        assert_eq!(n, 2, "duplicate mailbox copy must be collapsed to one record");
        // Cursor must advance to the highest ROWID scanned (3), even though
        // the duplicate (ROWID 2) was skipped.
        assert_eq!(max_cursor, 3, "cursor must advance past all scanned ROWIDs");

        let day = v.correspondence_timeline("2025-06-10").unwrap();
        assert_eq!(day.len(), 2);
        // The kept copy must carry the account owner as service.
        assert_eq!(day[0].guid, "<hello@msg.com>");
        assert_eq!(day[0].service, "david@wills.dev",
            "service must be account owner, not sender");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn uuid_from_imap_url_parses_correctly() {
        assert_eq!(uuid_from_imap_url("imap://UUID-AAA/INBOX"), Some("UUID-AAA"));
        assert_eq!(uuid_from_imap_url("imaps://9845A787/Sent%20Messages"), Some("9845A787"));
        assert_eq!(uuid_from_imap_url("imap://UUID-BBB/[Gmail]/All%20Mail"), Some("UUID-BBB"));
        assert_eq!(uuid_from_imap_url("imap:///nohost"), None);
        assert_eq!(uuid_from_imap_url(""), None);
    }

    #[test]
    fn strip_reply_prefix_handles_nested_re_and_brackets() {
        assert_eq!(strip_reply_prefix("Re: Lunch plans"), "Lunch plans");
        assert_eq!(strip_reply_prefix("Fwd: Re: Lunch plans"), "Lunch plans");
        assert_eq!(strip_reply_prefix("[ANNOUNCE] Re: Event"), "Event");
        assert_eq!(strip_reply_prefix("fw: Fw: FW: Meeting"), "Meeting");
        assert_eq!(strip_reply_prefix("No prefix here"), "No prefix here");
        assert_eq!(strip_reply_prefix(""), "");
    }

    #[test]
    fn sync_state_persisted_and_reloaded() {
        let v = temp_vault("state");
        let state = AppleMailSyncState {
            updated: "2025-06-10T12:00:00-07:00".into(),
            cursor: 42,
        };
        v.write_apple_mail_sync(&state).unwrap();
        let loaded = v.read_apple_mail_sync().unwrap();
        assert_eq!(loaded.cursor, 42);
        assert_eq!(loaded.updated, "2025-06-10T12:00:00-07:00");
    }

    #[test]
    fn missing_sync_state_returns_none() {
        let v = temp_vault("nostate");
        assert!(v.read_apple_mail_sync().is_none());
    }
}
