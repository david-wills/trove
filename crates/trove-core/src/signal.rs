//! Signal Desktop message archive — reads the local SQLCipher-encrypted
//! database (`~/Library/Application Support/Signal/sql/db.sqlite`).
//!
//! ## Access path
//!
//! Signal Desktop stores its SQLCipher encryption key two ways:
//!
//! - **Current (≥ mid-2024, PR #6849):** `config.json` holds an `encryptedKey`
//!   field (hex, prefixed with the bytes `v10`). The plaintext key is derived by
//!   PBKDF2-SHA1 (1003 iterations, salt `b"saltysalt"`, 16-byte output) using a
//!   Keychain password retrieved from the "Signal Safe Storage" generic-password
//!   item, then AES-128-CBC-decrypting the `encryptedKey` material. The result
//!   is the ASCII string passed to `PRAGMA key = 'x<hex>'`.
//!
//! - **Legacy (pre-2024):** `config.json` holds a plaintext `key` field — an
//!   ASCII hex string used directly as the SQLCipher key.
//!
//! Both are tried; Keychain access triggers a per-app approval dialog (owned by
//! macOS, not Trove). The collect hook degrades gracefully when the DB or key is
//! unavailable.
//!
//! ## Copy-then-read
//!
//! Signal may hold a WAL lock on the live database. We copy `db.sqlite` (and
//! `db.sqlite-wal` if present) to a temp path — never opening the original —
//! then open the copy with SQLCipher and delete it when done.
//!
//! ## Schema (Signal Desktop community docs)
//!
//! - `conversations` (type, id, serviceId, e164, name, profileName, members)
//! - `messages` (rowid, id, conversationId, type, body, sourceServiceId,
//!   timestamp, sent_at, serverTimestamp, hasAttachments, json)
//! - Attachments live in `json`; we extract only the count for the metadata.
//!
//! ## Vault output
//!
//! `correspondence/signal/YYYY-MM.jsonl`, one line per message/reaction.
//! Cursor persisted in `.trove/signal-sync.json` (max `ROWID` seen).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::import_via_copy;
use crate::correspondence::{AttachmentMeta, Message};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Integration registration

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_signal()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_messages > 0, || {
        format!("imported {} messages", s.new_messages)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    // Signal requires Full Disk Access (to read ~/Library/Application Support/Signal/)
    // plus Keychain access (prompted by macOS when the collector first runs).
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(signal_db_path().is_some_and(|p| fs::metadata(&p).is_ok())),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_signal_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "signal",
        name: "Signal",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Archives messages from Signal Desktop by reading the local \
                      encrypted database. The database key is stored in the macOS \
                      Keychain and requires explicit per-app approval the first time.",
        domain: "correspondence",
        vault_path: "correspondence/signal/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "When Trove first syncs Signal it will prompt you to approve Keychain access — approve it to allow the key read.",
            "Requires Signal Desktop to be installed and linked to your phone.",
        ],
        caveats: "Only messages present in Signal Desktop at sync time are captured. \
                 Disappearing messages vanish from the database before the next poll. \
                 Signal's database schema may shift with app updates; the collector \
                 fails soft and reports schema mismatches rather than crashing.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(900), // 15 minutes, same as iMessage
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Paths

fn signal_support_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/Application Support/Signal"))
}

fn signal_db_path() -> Option<PathBuf> {
    signal_support_dir().map(|d| d.join("sql/db.sqlite"))
}

fn signal_config_path() -> Option<PathBuf> {
    signal_support_dir().map(|d| d.join("config.json"))
}

// ---------------------------------------------------------------------------
// Sync state

const SYNC_FILE: &str = ".trove/signal-sync.json";

/// Incremental-sync state, persisted in `.trove/signal-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SignalSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest imported `messages.ROWID`.
    pub cursor: i64,
}

/// Result of one sync pass.
#[derive(Debug, Clone, Serialize)]
pub struct SignalSyncStats {
    /// False when the Signal DB or its key is unavailable — nothing attempted.
    pub available: bool,
    pub new_messages: u64,
}

// ---------------------------------------------------------------------------
// Key derivation

/// `config.json` shape — only the fields we care about.
#[derive(Deserialize)]
struct SignalConfig {
    /// Current format: AES-CBC-encrypted key hex, prefixed with `v10` bytes.
    #[serde(rename = "encryptedKey", default)]
    encrypted_key: String,
    /// Legacy format: plaintext hex key (no encryption layer).
    #[serde(default)]
    key: String,
}

/// Retrieve the password from the macOS Keychain under "Signal Safe Storage".
/// Returns None if the Keychain item doesn't exist or the user denies access.
fn keychain_password() -> Option<String> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-ws", "Signal Safe Storage"])
        .output()
        .ok()?;
    if out.status.success() {
        let pw = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if pw.is_empty() { None } else { Some(pw) }
    } else {
        None
    }
}

/// Derive the SQLCipher key from config.json + macOS Keychain.
///
/// Returns the key as a plain ASCII string (e.g. `"3f8a..."`) suitable for
/// `PRAGMA key = 'x<hex>'` if the result looks like hex, or as a literal
/// passphrase for `PRAGMA key = '...'` otherwise.
fn derive_signal_key(config: &SignalConfig) -> Option<String> {
    // Legacy: plaintext hex key in config.json
    if !config.key.is_empty() {
        return Some(config.key.clone());
    }
    // Current: encryptedKey (hex, v10-prefixed) + Keychain password
    if config.encrypted_key.is_empty() {
        return None;
    }
    let password = keychain_password()?;
    decrypt_signal_key(&config.encrypted_key, &password).ok()
}

/// Decrypt the `encryptedKey` hex value from config.json.
///
/// Algorithm (macOS path, matching Signal-Desktop PR #6849 + carderne/signal-export):
/// 1. hex-decode `encrypted_key_hex`
/// 2. strip 3-byte prefix `b"v10"` (byte values 0x76 0x31 0x30)
/// 3. PBKDF2-SHA1: key = PBKDF2(password, salt=b"saltysalt", dkLen=16, c=1003)
/// 4. AES-128-CBC-decrypt(iv=b" "*16, ciphertext=remainder) → padded plaintext
/// 5. strip PKCS#7 padding → ASCII key string
fn decrypt_signal_key(encrypted_key_hex: &str, password: &str) -> Result<String> {
    use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
    use pbkdf2::pbkdf2_hmac;
    use sha1::Sha1;

    let enc_bytes = hex::decode(encrypted_key_hex)
        .context("encryptedKey is not valid hex")?;

    // Strip the "v10" prefix (3 bytes: 0x76, 0x31, 0x30)
    const PREFIX: &[u8] = b"v10";
    if !enc_bytes.starts_with(PREFIX) {
        bail!("encryptedKey does not start with 'v10' prefix");
    }
    let ciphertext = &enc_bytes[PREFIX.len()..];
    if ciphertext.len() % 16 != 0 || ciphertext.is_empty() {
        bail!("ciphertext length {} is not a multiple of 16", ciphertext.len());
    }

    // PBKDF2-SHA1 → 16-byte AES key
    let salt = b"saltysalt";
    let mut aes_key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), salt, 1003, &mut aes_key);

    // AES-128-CBC decrypt with IV = 16 spaces (0x20)
    let iv = [0x20u8; 16];
    let cipher = aes::Aes128::new(GenericArray::from_slice(&aes_key));

    // Work on a separate plaintext buffer so we can read original ciphertext
    // for XOR without conflicting borrows.
    let mut plaintext = ciphertext.to_vec();
    let n_blocks = plaintext.len() / 16;
    for i in 0..n_blocks {
        let block_start = i * 16;
        let mut block = GenericArray::clone_from_slice(&plaintext[block_start..block_start + 16]);
        cipher.decrypt_block(&mut block);
        // XOR with previous ciphertext block (or IV for block 0); read from
        // the original `ciphertext` slice to avoid conflicting borrows.
        let prev: &[u8] = if i == 0 { &iv } else { &ciphertext[(i - 1) * 16..i * 16] };
        for j in 0..16 {
            block[j] ^= prev[j];
        }
        plaintext[block_start..block_start + 16].copy_from_slice(&block);
    }

    // PKCS#7 unpadding
    let pad = *plaintext.last().context("empty plaintext after decryption")? as usize;
    if pad == 0 || pad > 16 || pad > plaintext.len() {
        bail!("invalid PKCS#7 padding byte {pad}");
    }
    for &b in &plaintext[plaintext.len() - pad..] {
        if b as usize != pad {
            bail!("invalid PKCS#7 padding content");
        }
    }
    let result = plaintext[..plaintext.len() - pad].to_vec();
    String::from_utf8(result).context("decrypted key is not valid UTF-8")
}

// ---------------------------------------------------------------------------
// Database reader

/// Open a SQLCipher copy with the right key + pragmas, run `f`, return result.
fn with_signal_db<R>(db: &Path, key: &str, f: impl FnOnce(&rusqlite::Connection) -> Result<R>) -> Result<R> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening signal db copy {}", db.display()))?;

    // SQLCipher pragmas matching Signal Desktop's configuration (community-verified):
    // key is the plain ASCII string from derive_signal_key(), wrapped as x'hex' if hex
    // or as a quoted passphrase if plain text.
    let key_pragma = if key.chars().all(|c| c.is_ascii_hexdigit()) {
        format!("PRAGMA key = \"x'{}'\";", key)
    } else {
        format!("PRAGMA key = '{}';", key.replace('\'', "''"))
    };
    conn.execute_batch(&format!(
        "{key_pragma}
         PRAGMA cipher_page_size = 4096;
         PRAGMA kdf_iter = 64000;
         PRAGMA cipher_hmac_algorithm = HMAC_SHA512;
         PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;"
    ))?;

    f(&conn)
}

/// Core message-import logic — operates on an already-open `Connection`.
/// Extracted so tests can pass a plain (unencrypted) connection directly.
fn import_from_conn(vault: &Vault, conn: &rusqlite::Connection, cursor: i64) -> Result<(u64, i64)> {
    let convos = load_conversations(conn)?;

    let mut stmt = conn.prepare(
        "SELECT ROWID, id, conversationId, type,
                COALESCE(body, ''), COALESCE(sourceServiceId, ''),
                COALESCE(timestamp, 0), COALESCE(sent_at, 0),
                COALESCE(json, ''),
                COALESCE(hasAttachments, 0)
         FROM messages
         WHERE ROWID > ?1
         ORDER BY ROWID",
    )?;
    let mut rows = stmt.query([cursor])?;
    let mut messages = Vec::new();
    let mut max = cursor;
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        max = max.max(rowid);
        let msg_type: String = row.get(3)?;
        // Only import human conversation messages
        let from_me = match msg_type.as_str() {
            "outgoing" => true,
            "incoming" => false,
            _ => continue,
        };
        let guid: String = row.get(1)?;
        let convo_id: String = row.get(2)?;
        let body: String = row.get(4)?;
        let source_id: String = row.get(5)?;
        let ts_ms: i64 = row.get(6)?;
        let sent_at_ms: i64 = row.get(7)?;
        let json_col: String = row.get(8)?;
        let has_attachments: i64 = row.get(9)?;

        // Pick the best timestamp
        let ms = if from_me && sent_at_ms > 0 { sent_at_ms } else if ts_ms > 0 { ts_ms } else { continue };
        let Some(dt) = DateTime::from_timestamp_millis(ms) else { continue };
        let local: DateTime<Local> = dt.with_timezone(&Local);

        let (chat_handle, chat_name) = convos.get(&convo_id).cloned().unwrap_or_default();

        let mut m = Message::new("signal", local.to_rfc3339());
        m.guid = guid;
        m.rowid = rowid;
        m.from_me = from_me;
        m.service = "Signal".into();
        m.text = body;
        m.chat = chat_handle.clone();
        m.chat_name = chat_name;
        if !from_me {
            m.sender = if !chat_handle.is_empty() && !source_id.is_empty() {
                source_id
            } else {
                chat_handle
            };
        }
        if has_attachments > 0 && !json_col.is_empty() {
            m.attachments = parse_attachments_from_json(&json_col);
        }
        if m.text.is_empty() && m.attachments.is_empty() {
            continue;
        }
        messages.push(m);
    }
    vault.append_messages(&messages)?;
    Ok((messages.len() as u64, max))
}

/// Parse Signal's `messages.json` column for attachment metadata.
fn parse_attachments_from_json(json: &str) -> Vec<AttachmentMeta> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(arr) = v.get("attachments").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .map(|a| AttachmentMeta {
            name: a.get("fileName").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            mime: a.get("contentType").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            bytes: a.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
        })
        .collect()
}

/// Read conversations → build a map of conversationId → (e164/serviceId, display name).
fn load_conversations(conn: &rusqlite::Connection) -> Result<HashMap<String, (String, String)>> {
    let mut map = HashMap::new();
    // Schema: id (uuid), e164, serviceId, name, profileName
    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(e164, ''), COALESCE(serviceId, ''),
                COALESCE(name, ''), COALESCE(profileName, '')
         FROM conversations"
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let e164: String = row.get(1)?;
        let service_id: String = row.get(2)?;
        let name: String = row.get(3)?;
        let profile_name: String = row.get(4)?;
        // Prefer e164 (phone number) as the chat handle; fall back to serviceId (UUID/ACI)
        let handle = if !e164.is_empty() { e164 } else { service_id };
        // Prefer display name (group name / contact name) over profileName
        let display = if !name.is_empty() { name } else { profile_name };
        map.insert(id, (handle, display));
    }
    Ok(map)
}

/// Import Signal messages from an on-disk SQLCipher database.
/// Opens the DB with the SQLCipher key + pragmas, then delegates to
/// [`import_from_conn`].
fn import_signal_db(vault: &Vault, db: &Path, key: &str, cursor: i64) -> Result<(u64, i64)> {
    with_signal_db(db, key, |conn| {
        // Verify the key worked — a wrong key makes sqlite_master unreadable.
        let _: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .context("could not read sqlite_master — wrong key or corrupt DB")?;
        import_from_conn(vault, conn, cursor)
    })
}

impl Vault {
    /// One incremental sync pass over Signal Desktop's database.
    /// Silently a no-op when Signal is not installed or the DB is unreadable.
    pub fn collect_signal(&self) -> Result<SignalSyncStats> {
        let Some(db) = signal_db_path() else {
            return Ok(SignalSyncStats { available: false, new_messages: 0 });
        };
        if fs::metadata(&db).is_err() {
            return Ok(SignalSyncStats { available: false, new_messages: 0 });
        }

        // Read the encryption key
        let key = match self.signal_key() {
            Some(k) => k,
            None => return Ok(SignalSyncStats { available: false, new_messages: 0 }),
        };

        let mut state = self.read_signal_sync().unwrap_or_default();
        let stem = format!("trove-signal-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| {
            import_signal_db(self, tmp, &key, state.cursor)
        })?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_signal_sync(&state)?;
        Ok(SignalSyncStats { available: true, new_messages: n })
    }

    /// Derive the Signal encryption key from config.json + Keychain.
    fn signal_key(&self) -> Option<String> {
        let config_path = signal_config_path()?;
        let body = fs::read_to_string(&config_path).ok()?;
        let config: SignalConfig = serde_json::from_str(&body).ok()?;
        derive_signal_key(&config)
    }

    /// Read the persisted sync state.
    pub fn read_signal_sync(&self) -> Option<SignalSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_signal_sync(&self, state: &SignalSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Rebuild the cursor from stored JSONL files (used when sync file is lost).
    pub fn rebuild_signal_sync(&self) -> SignalSyncState {
        let mut cursor = 0i64;
        let dir = self.root().join("correspondence/signal");
        let Ok(entries) = fs::read_dir(&dir) else {
            return SignalSyncState::default();
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else { continue };
            for line in body.lines() {
                if let Ok(m) = serde_json::from_str::<Message>(line) {
                    cursor = cursor.max(m.rowid);
                }
            }
        }
        SignalSyncState { updated: String::new(), cursor }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-signal-{}-{name}", std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Collect all Signal messages from the vault JSONL files, independent of
    /// local timezone (avoids hardcoded date keys in assertions).
    fn read_all_signal_messages(vault: &Vault) -> Vec<Message> {
        let dir = vault.root().join("correspondence/signal");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else { return out };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") { continue }
            let Ok(body) = fs::read_to_string(&path) else { continue };
            for line in body.lines() {
                if let Ok(m) = serde_json::from_str::<Message>(line) {
                    out.push(m);
                }
            }
        }
        out.sort_by(|a, b| a.rowid.cmp(&b.rowid));
        out
    }

    /// Build a minimal plain-SQLite Signal DB (no SQLCipher key) for unit
    /// tests — the collect path needs SQLCipher at runtime, but we test the
    /// parser, cursor, and vault-write paths against a plain copy opened via
    /// plain rusqlite.
    fn fake_signal_db(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir().join(format!(
            "trove-signal-fake-{}-{name}.db", std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                type TEXT,
                e164 TEXT,
                serviceId TEXT,
                name TEXT,
                profileName TEXT
             );
             CREATE TABLE messages (
                ROWID INTEGER PRIMARY KEY,
                id TEXT,
                conversationId TEXT,
                type TEXT,
                body TEXT,
                sourceServiceId TEXT,
                timestamp INTEGER DEFAULT 0,
                sent_at INTEGER DEFAULT 0,
                serverTimestamp INTEGER DEFAULT 0,
                hasAttachments INTEGER DEFAULT 0,
                json TEXT
             );"
        ).unwrap();
        (path, conn)
    }

    // Fixed timestamp: 2026-06-10T12:00:00Z → ms
    const BASE_MS: i64 = 1749556800000; // 2026-06-10 12:00:00 UTC

    #[test]
    fn import_basic_messages_incremental() {
        let v = temp_vault("basic");
        let (db, conn) = fake_signal_db("basic");

        conn.execute_batch(
            "INSERT INTO conversations VALUES
                ('c1', 'private', '+15551234567', 'aci-abc', 'Alice', 'Alice P.'),
                ('c2', 'group',   NULL,           'aci-group', 'The Group', NULL);",
        ).unwrap();

        // incoming from Alice
        conn.execute(
            "INSERT INTO messages (ROWID, id, conversationId, type, body, sourceServiceId, timestamp, sent_at)
             VALUES (1, 'msg-1', 'c1', 'incoming', 'Hey there!', 'aci-abc', ?1, 0)",
            [BASE_MS],
        ).unwrap();
        // outgoing reply
        conn.execute(
            "INSERT INTO messages (ROWID, id, conversationId, type, body, sent_at, timestamp)
             VALUES (2, 'msg-2', 'c1', 'outgoing', 'Hi Alice!', ?1, 0)",
            [BASE_MS + 60000],
        ).unwrap();
        // group incoming
        conn.execute(
            "INSERT INTO messages (ROWID, id, conversationId, type, body, sourceServiceId, timestamp)
             VALUES (3, 'msg-3', 'c2', 'incoming', 'Group hello', 'aci-xyz', ?1)",
            [BASE_MS + 120000],
        ).unwrap();
        // system message — should be skipped
        conn.execute(
            "INSERT INTO messages (ROWID, id, conversationId, type, body, timestamp)
             VALUES (4, 'msg-4', 'c1', 'call-history', 'call', ?1)",
            [BASE_MS + 180000],
        ).unwrap();
        // empty body with no attachments — should be skipped
        conn.execute(
            "INSERT INTO messages (ROWID, id, conversationId, type, body, timestamp)
             VALUES (5, 'msg-5', 'c1', 'incoming', '', ?1)",
            [BASE_MS + 200000],
        ).unwrap();

        // Use import_from_conn directly with the plain-SQLite connection —
        // tests never set a SQLCipher key; the SQLCipher pragma path is
        // exercised only against a real Signal DB.
        let (n, cursor) = import_from_conn(&v, &conn, 0).unwrap();
        assert_eq!(n, 3, "system message and empty message are skipped");
        assert_eq!(cursor, 5, "cursor advances past skipped rows");

        // Read all signal messages from vault — timezone-safe (no date-hardcoding)
        let all_msgs = read_all_signal_messages(&v);
        assert_eq!(all_msgs.len(), 3);

        // Messages are ordered by timestamp (ascending)
        let incoming = all_msgs.iter().find(|m| m.guid == "msg-1").expect("msg-1");
        assert_eq!(incoming.source, "signal");
        assert_eq!(incoming.text, "Hey there!");
        assert!(!incoming.from_me);
        assert_eq!(incoming.chat, "+15551234567");
        assert_eq!(incoming.service, "Signal");
        assert_eq!(incoming.rowid, 1);

        let outgoing = all_msgs.iter().find(|m| m.guid == "msg-2").expect("msg-2");
        assert!(outgoing.from_me);
        assert_eq!(outgoing.text, "Hi Alice!");
        assert_eq!(outgoing.chat, "+15551234567");

        let group_msg = all_msgs.iter().find(|m| m.guid == "msg-3").expect("msg-3");
        assert_eq!(group_msg.chat_name, "The Group");
        assert_eq!(group_msg.text, "Group hello");

        // Incrementality: re-import from cursor should yield nothing new
        let (n2, cursor2) = import_from_conn(&v, &conn, cursor).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(cursor2, cursor);
        assert_eq!(read_all_signal_messages(&v).len(), 3, "no duplicates on re-import");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn attachment_metadata_from_json() {
        let v = temp_vault("attach");
        let (db, conn) = fake_signal_db("attach");

        conn.execute_batch(
            "INSERT INTO conversations VALUES ('c1', 'private', '+1555', 'aci', 'Bob', NULL);"
        ).unwrap();

        let json = r#"{"attachments":[{"fileName":"photo.jpg","contentType":"image/jpeg","size":204800}]}"#;
        conn.execute(
            "INSERT INTO messages (ROWID, id, conversationId, type, body, timestamp, hasAttachments, json)
             VALUES (1, 'a1', 'c1', 'incoming', '', ?1, 1, ?2)",
            rusqlite::params![BASE_MS, json],
        ).unwrap();

        let (n, _) = import_from_conn(&v, &conn, 0).unwrap();
        assert_eq!(n, 1, "attachment-only message kept");

        // Read messages directly from JSONL — timezone-safe
        let msgs = read_all_signal_messages(&v);
        assert_eq!(msgs.len(), 1, "one message in the vault JSONL");
        assert_eq!(msgs[0].attachments.len(), 1);
        assert_eq!(msgs[0].attachments[0].name, "photo.jpg");
        assert_eq!(msgs[0].attachments[0].mime, "image/jpeg");
        assert_eq!(msgs[0].attachments[0].bytes, 204800);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn cursor_rebuilt_from_jsonl() {
        let v = temp_vault("rebuild");
        let (db, conn) = fake_signal_db("rebuild");
        conn.execute_batch(
            "INSERT INTO conversations VALUES ('c1', 'private', '+1555', 'aci', NULL, NULL);"
        ).unwrap();
        conn.execute(
            "INSERT INTO messages (ROWID, id, conversationId, type, body, timestamp)
             VALUES (7, 'r1', 'c1', 'incoming', 'hello', ?1)",
            [BASE_MS],
        ).unwrap();

        let (n, _) = import_from_conn(&v, &conn, 0).unwrap();
        assert_eq!(n, 1);
        // Drop sync file to simulate loss
        let sync_path = v.resolve(SYNC_FILE).unwrap();
        let _ = fs::remove_file(&sync_path);
        assert!(v.read_signal_sync().is_none());
        let rebuilt = v.rebuild_signal_sync();
        assert_eq!(rebuilt.cursor, 7, "rebuild reads rowid=7 from stored JSONL");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn decrypt_signal_key_round_trip() {
        // Build a known ciphertext from the algorithm and verify decryption.
        // plaintext = "test-signal-key-1" (17 bytes → padded to 32 with PKCS#7 = 15 0x0f bytes)
        use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
        use pbkdf2::pbkdf2_hmac;
        use sha1::Sha1;

        let password = "test-keychain-password";
        let salt = b"saltysalt";
        let mut aes_key = [0u8; 16];
        pbkdf2_hmac::<Sha1>(password.as_bytes(), salt, 1003, &mut aes_key);

        let plaintext = b"test-signal-key-1";
        // PKCS#7 pad to 32 bytes
        let pad_len = 32 - plaintext.len();
        let mut padded = plaintext.to_vec();
        padded.extend(std::iter::repeat(pad_len as u8).take(pad_len));

        // AES-128-CBC encrypt with IV = 16 spaces
        let iv = [0x20u8; 16];
        let cipher = aes::Aes128::new(GenericArray::from_slice(&aes_key));
        let mut ciphertext = padded.clone();
        let n_blocks = ciphertext.len() / 16;
        for i in 0..n_blocks {
            // XOR with IV or previous ciphertext block
            let xor_start = if i == 0 { usize::MAX } else { (i - 1) * 16 };
            for j in 0..16 {
                let xor = if i == 0 { iv[j] } else { ciphertext[xor_start + j] };
                ciphertext[i * 16 + j] ^= xor;
            }
            let mut block = GenericArray::clone_from_slice(&ciphertext[i * 16..i * 16 + 16]);
            cipher.encrypt_block(&mut block);
            ciphertext[i * 16..i * 16 + 16].copy_from_slice(&block);
        }

        // Prepend "v10" prefix and hex-encode
        let mut enc_bytes = b"v10".to_vec();
        enc_bytes.extend_from_slice(&ciphertext);
        let encrypted_hex = hex::encode(&enc_bytes);

        // Now decrypt and verify
        let decrypted = decrypt_signal_key(&encrypted_hex, password).unwrap();
        assert_eq!(decrypted, "test-signal-key-1");
    }

    #[test]
    fn parse_attachments_from_json_handles_missing() {
        // No attachments field
        let metas = parse_attachments_from_json(r#"{"body":"hi"}"#);
        assert!(metas.is_empty());
        // Invalid JSON
        let metas2 = parse_attachments_from_json("not json");
        assert!(metas2.is_empty());
        // Valid attachment
        let metas3 = parse_attachments_from_json(
            r#"{"attachments":[{"fileName":"f.pdf","contentType":"application/pdf","size":1024}]}"#
        );
        assert_eq!(metas3.len(), 1);
        assert_eq!(metas3[0].name, "f.pdf");
    }
}
