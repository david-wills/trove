//! iMessage / SMS / RCS collector — an M3 ("copy-then-read another app's
//! database") source over `~/Library/Messages/chat.db`, modeled on
//! [`crate::browser`]. Needs **Full Disk Access** (same per-binary,
//! no-programmatic-prompt grant as Safari history); silently skipped while
//! the DB is unreadable.
//!
//! Writes the unified correspondence stream (see [`crate::correspondence`]):
//! `correspondence/imessage/YYYY-MM.jsonl`, one message per line. Tapbacks
//! are stored as `kind:"reaction"` (with the target message's guid in
//! `reply_to`), group renames/membership changes as `kind:"event"` — full
//! fidelity at write time; reads decide what counts as conversation volume.
//!
//! **Text lives in typedstream blobs.** Modern macOS leaves `message.text`
//! NULL (on this machine: 38 of 89,761 rows had plain text) and stores the
//! body in `attributedBody`, an NSArchiver "typedstream" of the
//! NSAttributedString. We extract the backing string with a deliberately
//! narrow parser: find the `NSString`/`NSMutableString` class name, then the
//! next `0x84 0x01 '+'` type tag, read a typedstream integer length, then
//! that many UTF-8 bytes. Validated against all 89,429 blobs in the real DB
//! (100% decode, zero failures). Reference for the full format:
//! ReagentX/imessage-exporter's typedstream docs (we deliberately do not
//! depend on that crate — GPL).
//!
//! Incremental sync cursors on **ROWID**, not date: Messages-in-iCloud can
//! insert rows whose `date` is years old (device sync backfill), and those
//! still get fresh ROWIDs — a date cursor would miss them. Every JSONL line
//! carries its `rowid`, so a lost `.trove/imessage-sync.json` is rebuilt by
//! scanning the logs, exactly like the browser cursors.

use std::collections::HashMap;
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

/// Seconds between iMessage syncs in the watcher loop.
pub const IMESSAGE_SYNC_SECS: u64 = 900;

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_imessages()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_messages > 0, || {
        format!("imported {} messages", s.new_messages)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(imessage_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_imessage_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "imessage",
        name: "Messages (iMessage)",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Syncs iMessage/SMS/RCS from Messages every 15 minutes; the first sync backfills the full retained history.",
        domain: "correspondence",
        vault_path: "correspondence/imessage/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats: "Senders are raw handles (phone numbers, addresses) until the planned Contacts source maps them to people.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(IMESSAGE_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

const SYNC_FILE: &str = ".trove/imessage-sync.json";

/// Seconds between the Unix epoch and the Apple/Core Data epoch
/// (2001-01-01). `message.date` is nanoseconds since 2001, UTC (legacy
/// pre-High Sierra rows used whole seconds — handled).
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

/// Incremental-sync state, persisted in `.trove/imessage-sync.json`.
/// Rebuildable from the JSONL logs (see [`Vault::rebuild_imessage_sync`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct IMessageSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest imported `message.ROWID`.
    pub cursor: i64,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Serialize)]
pub struct IMessageSyncStats {
    /// False when chat.db is unreadable (no Full Disk Access, or Messages
    /// never used) — nothing was attempted.
    pub available: bool,
    pub new_messages: u64,
}

fn imessage_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/Messages/chat.db"))
}

/// Whether this process can read the Messages database. False means Full
/// Disk Access hasn't been granted to this binary (or Messages has never
/// run). There is no API to prompt for FDA — the UI deep-links to System
/// Settings, same as Safari history.
pub fn imessage_permission_ok() -> bool {
    imessage_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// typedstream text extraction

/// First offset of `needle` in `hay` at or after `from`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// A typedstream integer at `i`: values below 0x81 are inline; 0x81 / 0x82
/// prefix little-endian u16 / u32. Returns (value, next offset).
fn typedstream_int(buf: &[u8], i: usize) -> Option<(usize, usize)> {
    match *buf.get(i)? {
        b if b < 0x81 => Some((b as usize, i + 1)),
        0x81 => {
            let v = u16::from_le_bytes(buf.get(i + 1..i + 3)?.try_into().ok()?);
            Some((v as usize, i + 3))
        }
        0x82 => {
            let v = u32::from_le_bytes(buf.get(i + 1..i + 5)?.try_into().ok()?);
            Some((v as usize, i + 5))
        }
        _ => None,
    }
}

/// The attributed string's backing text out of an `attributedBody`
/// typedstream blob. None when the blob holds no string (or is malformed) —
/// callers fall back to empty text, never an error.
pub(crate) fn typedstream_text(blob: &[u8]) -> Option<String> {
    let cls = find(blob, b"NSString", 0).or_else(|| find(blob, b"NSMutableString", 0))?;
    // 0x84 0x01 '+' — "one value follows, of type '+' (raw data)".
    let tag = find(blob, b"\x84\x01+", cls)?;
    let (len, j) = typedstream_int(blob, tag + 3)?;
    let bytes = blob.get(j..j + len)?;
    std::str::from_utf8(bytes).map(str::to_owned).ok()
}

// ---------------------------------------------------------------------------
// chat.db semantics

/// `message.date` (ns since 2001, UTC; legacy rows: whole seconds) → local.
fn apple_time_to_local(date: i64) -> Option<DateTime<Local>> {
    if date <= 0 {
        return None;
    }
    // Legacy second-precision rows are ~1e9; nanosecond rows are ~1e17.
    let ns = if date < 1_000_000_000_000 {
        date.checked_mul(1_000_000_000)?
    } else {
        date
    };
    let unix_ns = ns.checked_add(APPLE_EPOCH_OFFSET_S.checked_mul(1_000_000_000)?)?;
    let t = DateTime::from_timestamp(unix_ns.div_euclid(1_000_000_000), unix_ns.rem_euclid(1_000_000_000) as u32)?;
    Some(t.with_timezone(&Local))
}

/// Tapback `associated_message_type` → reaction name. 2000s are additions,
/// 3000s removals.
fn reaction_name(t: i64) -> Option<String> {
    let (base, removed) = match t {
        2000..=2007 => (t - 2000, false),
        3000..=3007 => (t - 3000, true),
        _ => return None,
    };
    let name = match base {
        0 => "loved",
        1 => "liked",
        2 => "disliked",
        3 => "laughed",
        4 => "emphasized",
        5 => "questioned",
        6 => "emoji",
        _ => "sticker",
    };
    Some(if removed { format!("removed-{name}") } else { name.into() })
}

/// `associated_message_guid` ("p:0/GUID", "bp:GUID", or bare) → target guid.
fn reaction_target(guid: &str) -> String {
    let g = guid.rsplit_once('/').map_or(guid, |(_, g)| g);
    g.strip_prefix("bp:").unwrap_or(g).to_string()
}

// ---------------------------------------------------------------------------
// the import

/// Read messages with ROWID > `cursor` out of a chat.db (or a copy) and
/// append them to the vault. Returns (rows imported, new cursor). Split from
/// the copy step so tests can run it on a synthetic DB.
fn import_imessage_db(vault: &Vault, db: &Path, cursor: i64) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening chat.db copy {}", db.display()))?;

    // Attachment metadata for the rows this pass will touch.
    let mut attachments: HashMap<i64, Vec<AttachmentMeta>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT j.message_id, COALESCE(a.transfer_name, ''),
                    COALESCE(a.mime_type, ''), COALESCE(a.total_bytes, 0)
             FROM message_attachment_join j
             JOIN attachment a ON a.ROWID = j.attachment_id
             WHERE j.message_id > ?1",
        )?;
        let mut rows = stmt.query([cursor])?;
        while let Some(row) = rows.next()? {
            attachments.entry(row.get(0)?).or_default().push(AttachmentMeta {
                name: row.get(1)?,
                mime: row.get(2)?,
                bytes: row.get(3)?,
            });
        }
    }

    // GROUP BY collapses the rare message that belongs to several merged
    // chats down to one arbitrary chat — better than duplicating the line.
    let mut stmt = conn.prepare(
        "SELECT m.ROWID, m.guid, m.date, m.is_from_me, COALESCE(m.service, ''),
                m.text, m.attributedBody, m.associated_message_type,
                COALESCE(m.associated_message_guid, ''), m.item_type,
                COALESCE(m.group_title, ''), COALESCE(m.thread_originator_guid, ''),
                COALESCE(h.id, ''), COALESCE(c.chat_identifier, ''),
                COALESCE(c.display_name, '')
         FROM message m
         LEFT JOIN handle h ON h.ROWID = m.handle_id
         LEFT JOIN chat_message_join j ON j.message_id = m.ROWID
         LEFT JOIN chat c ON c.ROWID = j.chat_id
         WHERE m.ROWID > ?1
         GROUP BY m.ROWID
         ORDER BY m.ROWID",
    )?;
    let mut rows = stmt.query([cursor])?;
    let mut messages = Vec::new();
    let mut max = cursor;
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        max = max.max(rowid);
        let Some(local) = apple_time_to_local(row.get(2)?) else {
            continue;
        };
        let plain: Option<String> = row.get(5)?;
        let blob: Option<Vec<u8>> = row.get(6)?;
        // U+FFFC marks where an attachment sits inside the attributed string
        // — meaningless once attachments are their own field.
        let text = plain
            .filter(|t| !t.is_empty())
            .or_else(|| blob.as_deref().and_then(typedstream_text))
            .unwrap_or_default()
            .replace('\u{FFFC}', "")
            .trim()
            .to_string();

        let assoc_type: i64 = row.get(7)?;
        let item_type: i64 = row.get(9)?;
        let group_title: String = row.get(10)?;
        let mut m = Message::new("imessage", local.to_rfc3339());
        m.guid = row.get(1)?;
        m.rowid = rowid;
        m.from_me = row.get::<_, i64>(3)? != 0;
        m.service = row.get(4)?;
        m.text = text;
        m.chat = row.get(13)?;
        m.chat_name = row.get(14)?;
        if !m.from_me {
            m.sender = row.get(12)?;
        }
        m.attachments = attachments.remove(&rowid).unwrap_or_default();
        if let Some(name) = reaction_name(assoc_type) {
            m.kind = "reaction".into();
            m.reaction = name;
            m.reply_to = reaction_target(&row.get::<_, String>(8)?);
        } else if item_type != 0 {
            // Group renames, membership changes, etc. The rename carries the
            // new title; other events keep whatever text decoded (often none).
            m.kind = "event".into();
            if !group_title.is_empty() && m.text.is_empty() {
                m.text = group_title;
            }
        } else {
            let thread: String = row.get(11)?;
            m.reply_to = thread;
        }
        // Rows with nothing to say (no text, no attachments, not a reaction
        // or event) are link-preview shells and sync artifacts — the cursor
        // still advances past them.
        if m.kind == "message" && m.text.is_empty() && m.attachments.is_empty() {
            continue;
        }
        messages.push(m);
    }
    vault.append_messages(&messages)?;
    Ok((messages.len() as u64, max))
}

impl Vault {
    /// One incremental sync pass over the Messages database. Silently a
    /// no-op (with `available:false`) while chat.db is unreadable — the UI
    /// surfaces the permission state; logging every pass would be noise.
    pub fn collect_imessages(&self) -> Result<IMessageSyncStats> {
        if !imessage_permission_ok() {
            return Ok(IMessageSyncStats {
                available: false,
                new_messages: 0,
            });
        }
        let db = imessage_db_path().expect("permission_ok implies path");
        let mut state = match self.read_imessage_sync() {
            Some(s) => s,
            None => self.rebuild_imessage_sync(),
        };
        let stem = format!("trove-imessage-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| {
            import_imessage_db(self, tmp, state.cursor)
        })?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_imessage_sync(&state)?;
        Ok(IMessageSyncStats {
            available: true,
            new_messages: n,
        })
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_imessage_sync(&self) -> Option<IMessageSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_imessage_sync(&self, state: &IMessageSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Reconstruct the cursor from the JSONL logs — used when the sync file
    /// is missing so a resync appends only genuinely new messages.
    fn rebuild_imessage_sync(&self) -> IMessageSyncState {
        let mut cursor = 0i64;
        let dir = self.root().join("correspondence/imessage");
        let Ok(entries) = fs::read_dir(&dir) else {
            return IMessageSyncState::default();
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            for line in body.lines() {
                if let Ok(m) = serde_json::from_str::<Message>(line) {
                    cursor = cursor.max(m.rowid);
                }
            }
        }
        IMessageSyncState {
            updated: String::new(),
            cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-imessage-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a typedstream blob the way Messages does, around `text`.
    fn fake_typedstream(text: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+");
        let bytes = text.as_bytes();
        match bytes.len() {
            n if n < 0x81 => b.push(n as u8),
            n if n <= u16::MAX as usize => {
                b.push(0x81);
                b.extend_from_slice(&(n as u16).to_le_bytes());
            }
            n => {
                b.push(0x82);
                b.extend_from_slice(&(n as u32).to_le_bytes());
            }
        }
        b.extend_from_slice(bytes);
        b.extend_from_slice(b"\x86\x84\x02iI\x01\x92\x86");
        b
    }

    /// `message.date` (ns since 2001) for a fixed local datetime.
    fn apple_ns(d: u32, h: u32) -> i64 {
        let t = Local.with_ymd_and_hms(2026, 6, d, h, 0, 0).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) * 1_000_000_000
    }

    fn fake_chat_db(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir().join(format!(
            "trove-fakechatdb-{}-{name}.db",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, date INTEGER,
                is_from_me INTEGER DEFAULT 0, service TEXT, text TEXT, attributedBody BLOB,
                associated_message_type INTEGER DEFAULT 0, associated_message_guid TEXT,
                item_type INTEGER DEFAULT 0, group_title TEXT, thread_originator_guid TEXT,
                handle_id INTEGER DEFAULT 0);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT,
                display_name TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY, transfer_name TEXT,
                mime_type TEXT, total_bytes INTEGER DEFAULT 0);
             CREATE TABLE message_attachment_join (message_id INTEGER, attachment_id INTEGER);",
        )
        .unwrap();
        (path, conn)
    }

    #[test]
    fn typedstream_round_trip() {
        // Short, multibyte, and length-encoded (>0x80 and >u16-ish) strings.
        for text in [
            "hey!",
            "Liked “All good here! 👍”",
            &"x".repeat(200),
            &"长".repeat(40_000),
        ] {
            let blob = fake_typedstream(text);
            assert_eq!(typedstream_text(&blob).as_deref(), Some(text));
        }
        assert_eq!(typedstream_text(b"garbage"), None);
        assert_eq!(typedstream_text(b""), None);
    }

    #[test]
    fn apple_epoch_conversion() {
        // 2026-06-10T00:00:00Z in Apple ns.
        let ns = (1_781_049_600 - APPLE_EPOCH_OFFSET_S) * 1_000_000_000;
        assert_eq!(apple_time_to_local(ns).unwrap().timestamp(), 1_781_049_600);
        // Legacy whole-second rows.
        assert_eq!(
            apple_time_to_local(1_781_049_600 - APPLE_EPOCH_OFFSET_S).unwrap().timestamp(),
            1_781_049_600
        );
        assert!(apple_time_to_local(0).is_none());
    }

    #[test]
    fn reactions_map_to_names_and_targets() {
        assert_eq!(reaction_name(2001).as_deref(), Some("liked"));
        assert_eq!(reaction_name(3000).as_deref(), Some("removed-loved"));
        assert_eq!(reaction_name(0), None);
        assert_eq!(reaction_name(1000), None);
        assert_eq!(reaction_target("p:0/ABC-123"), "ABC-123");
        assert_eq!(reaction_target("bp:DEF-456"), "DEF-456");
        assert_eq!(reaction_target("GHI-789"), "GHI-789");
    }

    #[test]
    fn import_decodes_joins_and_is_incremental() {
        let v = temp_vault("import");
        let (db, conn) = fake_chat_db("import");
        conn.execute_batch(
            "INSERT INTO handle VALUES (1, '+15551234567');
             INSERT INTO chat VALUES (1, '+15551234567', NULL), (2, 'chat99', 'The Group');",
        )
        .unwrap();
        // Incoming with typedstream body; outgoing with plain text; a
        // tapback; a group event; an empty shell row (skipped, cursor moves).
        conn.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me, service, attributedBody, handle_id)
             VALUES (1, 'G1', ?1, 0, 'iMessage', ?2, 1)",
            rusqlite::params![apple_ns(9, 10), fake_typedstream("incoming body")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me, service, text)
             VALUES (2, 'G2', ?1, 1, 'SMS', 'sent reply')",
            [apple_ns(10, 9)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me, service, attributedBody,
                                  associated_message_type, associated_message_guid, handle_id)
             VALUES (3, 'G3', ?1, 0, 'iMessage', ?2, 2001, 'p:0/G2', 1)",
            rusqlite::params![apple_ns(10, 10), fake_typedstream("Liked “sent reply”")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me, item_type, group_title)
             VALUES (4, 'G4', ?1, 0, 2, 'New Group Name')",
            [apple_ns(10, 11)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me) VALUES (5, 'G5', ?1, 0)",
            [apple_ns(10, 12)],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO chat_message_join VALUES (1, 1), (1, 2), (1, 3), (2, 4), (1, 5);",
        )
        .unwrap();

        let (n, cursor) = import_imessage_db(&v, &db, 0).unwrap();
        assert_eq!(n, 4, "shell row is skipped");
        assert_eq!(cursor, 5, "cursor advances past skipped rows");

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3);
        assert_eq!(day[0].text, "sent reply");
        assert!(day[0].from_me);
        assert_eq!(day[0].sender, "");
        assert_eq!(day[1].kind, "reaction");
        assert_eq!(day[1].reaction, "liked");
        assert_eq!(day[1].reply_to, "G2");
        assert_eq!(day[1].sender, "+15551234567");
        assert_eq!(day[2].kind, "event");
        assert_eq!(day[2].text, "New Group Name");
        assert_eq!(day[2].chat_name, "The Group");

        let prev = v.correspondence_timeline("2026-06-09").unwrap();
        assert_eq!(prev.len(), 1);
        assert_eq!(prev[0].text, "incoming body");
        assert_eq!(prev[0].chat, "+15551234567");

        // Re-import from the cursor: nothing new, nothing duplicated.
        let (n2, cursor2) = import_imessage_db(&v, &db, cursor).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(cursor2, cursor);

        // Cursor survives losing the sync file.
        assert_eq!(v.rebuild_imessage_sync().cursor, 4, "rebuild sees max stored rowid");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn attachments_carry_metadata() {
        let v = temp_vault("attach");
        let (db, conn) = fake_chat_db("attach");
        conn.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me) VALUES (1, 'A1', ?1, 1)",
            [apple_ns(10, 9)],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO attachment VALUES (1, 'IMG_0001.heic', 'image/heic', 123456);
             INSERT INTO message_attachment_join VALUES (1, 1);",
        )
        .unwrap();

        let (n, _) = import_imessage_db(&v, &db, 0).unwrap();
        assert_eq!(n, 1, "attachment-only message is kept");
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day[0].attachments.len(), 1);
        assert_eq!(day[0].attachments[0].name, "IMG_0001.heic");
        assert_eq!(day[0].attachments[0].mime, "image/heic");

        let _ = fs::remove_file(db);
    }
}
