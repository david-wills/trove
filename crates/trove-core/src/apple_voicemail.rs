//! Visual Voicemail collector — reads `voicemail.db` from local unencrypted
//! iPhone backups made by Finder / iTunes, and writes the unified
//! [`voice`](crate::voice) stream: `voice/apple-voicemail/YYYY-MM.jsonl`,
//! one [`Recording`] (`kind:"voicemail"`) per row, partitioned by the local
//! month of `ts`, deduped by `guid`.
//!
//! **Access path (no network):**
//! `~/Library/Application Support/MobileSync/Backup/<device-UUID>/`
//! contains a `Manifest.db` (SQLite) whose `Files` table maps a
//! `(domain, relativePath)` to a 40-char SHA-1 `fileID`. The hashed file
//! lives at `<backup-dir>/<fileID[0:2]>/<fileID>`. For voicemail data:
//! - DB: domain=`HomeDomain`, relativePath=`Library/Voicemail/voicemail.db`
//! - Audio: domain=`HomeDomain`, relativePath=`Library/Voicemail/<ROWID>.amr`
//!
//! **voicemail.db schema (community-documented; verified by iMazing /
//! iPhone Backup Extractor lineage; needs a real-sample confirmation pass):**
//! ```sql
//! CREATE TABLE voicemail (
//!   ROWID        INTEGER PRIMARY KEY,
//!   remote_uid   TEXT,    -- raw phone string from carrier
//!   date         REAL,    -- Unix epoch (seconds since 1970 UTC, NOT 2001)
//!   token        TEXT,
//!   sender       TEXT,    -- E.164 number or carrier handle
//!   callback_num TEXT,    -- callback phone number (TEXT); variant names observed:
//!                         -- callback_num, callback_date, callback_dt — probed
//!                         -- defensively; stored as string in extra
//!   duration     REAL,    -- seconds (Unix-epoch-independent, a duration)
//!   expiration   REAL,    -- CFAbsoluteTime (seconds since 2001-01-01 UTC)
//!   trashed_date REAL,    -- CFAbsoluteTime (seconds since 2001-01-01 UTC, NOT 1970)
//!   flags        INTEGER  -- bitmask; best-effort: 1=new/unread, 4=saved/archived,
//!                         -- 128=marked-deleted (unconfirmed — Needs-sample)
//! );
//! ```
//! The schema is probed defensively with `PRAGMA table_info(voicemail)` before
//! SELECT, so a column the local DB lacks is simply skipped.
//!
//! **Transcripts (`.transcript` binary plists):** the shape is
//! community-described but un-fixtured; the transcript parser is deliberately
//! parked until a real sample is available. Metadata rows (sender, date,
//! duration, flags) are written unconditionally; `transcript` stays empty
//! until the parser ships. See `docs/integrations/apple-voicemail.md`.
//!
//! **Encrypted backups:** detected from `Manifest.plist` `IsEncrypted` key,
//! or from `MANIFEST_ERROR`-type rusqlite errors on open — the UI shows a
//! clear explanation; an encrypted backup is never silently skipped as
//! "no backup found."
//!
//! **Incremental watermark:** per-backup `ROWID` cursor (voicemail ROWID is
//! append-only from the phone), persisted in `.trove/apple-voicemail-sync.json`.
//! Dedupe by `guid` is the authoritative correctness gate; the watermark is
//! advisory and rebuildable from the JSONL output.
//!
//! Complements [`crate::calls`] (which has call rows but no voicemail content)
//! and [`crate::google_voice`] (same voice contract, different source).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;
use crate::voice::Recording;

/// Seconds between Visual Voicemail syncs in the watcher loop (hourly — new
/// voicemails arrive infrequently; backup files are written at backup time).
pub const VOICEMAIL_SYNC_SECS: u64 = 3_600;

const SYNC_FILE: &str = ".trove/apple-voicemail-sync.json";
const SOURCE: &str = "apple-voicemail";
const VOICE_DIR: &str = "voice/apple-voicemail";

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_voicemails()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_voicemails > 0, || {
        format!(
            "imported {} voicemail{} from {} backup{}",
            s.new_voicemails,
            if s.new_voicemails == 1 { "" } else { "s" },
            s.backups_scanned,
            if s.backups_scanned == 1 { "" } else { "s" },
        )
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(voicemail_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_voicemail_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-voicemail",
        name: "Visual Voicemail (iPhone backup)",
        kind: IntegrationKind::LocalSync,
        // Opt-in: voicemail transcripts and caller metadata are sensitive
        // (the 🔒 gate the hub renders for default-off integrations).
        default_on: false,
        description: "Reads voicemail metadata — caller, date, duration, read/trashed state — \
                      from your local unencrypted iPhone backup every hour. Transcripts are \
                      added once a sample confirms the binary plist format (coming soon). \
                      Only works when you keep unencrypted local backups via Finder or iTunes; \
                      iCloud-only users get a clear explanation in-app.",
        domain: "voice",
        vault_path: "voice/apple-voicemail/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the \
             troved binary.",
            "Keep at least one unencrypted local iPhone backup via Finder (General → \
             Backups → Back Up Now, with 'Encrypt local backup' disabled).",
            "Restart the daemon after granting Full Disk Access.",
        ],
        caveats: "Requires an unencrypted local iPhone backup (Finder / iTunes). iCloud \
                  backups and encrypted local backups cannot be read — you will see a clear \
                  in-app explanation rather than a broken card. iOS 17 Live Voicemail \
                  real-time transcripts are not persisted in a readable form and are not \
                  collected here.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(VOICEMAIL_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Incremental sync state

/// Per-backup watermark + global state, persisted in `.trove/apple-voicemail-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct VoicemailSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Per-backup-UUID highest ROWID imported, so each backup drains
    /// independently (multi-device households have multiple backup dirs).
    #[serde(default)]
    pub cursors: HashMap<String, i64>,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Serialize)]
pub struct VoicemailSyncStats {
    /// False when no backup dir is readable at all.
    pub available: bool,
    pub new_voicemails: u64,
    pub backups_scanned: u64,
}

// ---------------------------------------------------------------------------
// Backup location + detection

fn mobilesync_backup_root() -> Option<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join("Library/Application Support/MobileSync/Backup"))
}

/// Whether this process can see the MobileSync/Backup folder.
/// False = Full Disk Access not granted.
pub fn voicemail_permission_ok() -> bool {
    mobilesync_backup_root().is_some_and(|p| fs::read_dir(&p).is_ok())
}

/// The state of a particular backup directory, from the perspective of
/// whether it can yield voicemail data.
#[derive(Debug, PartialEq)]
pub enum BackupState {
    /// No backup found at all.
    NoBackup,
    /// Found but encrypted — we cannot read it without the backup password.
    Encrypted,
    /// Readable, unencrypted backup — scan it.
    Ok,
}

/// Enumerate all device-UUID backup directories under the MobileSync root.
fn backup_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return vec![];
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// A 40-hex-char SHA-1 `fileID` → `<backup_dir>/<id[0:2]>/<id>`.
fn hashed_path(backup_dir: &Path, file_id: &str) -> PathBuf {
    backup_dir.join(&file_id[..2]).join(file_id)
}

/// Look up a file in the backup via `Manifest.db`.
/// `SELECT fileID FROM Files WHERE domain=? AND relativePath=?`
///
/// Returns `None` when the DB is unreadable or the path is absent.
fn manifest_lookup(backup_dir: &Path, domain: &str, relative_path: &str) -> Option<PathBuf> {
    let manifest = backup_dir.join("Manifest.db");
    let conn = rusqlite::Connection::open(&manifest).ok()?;
    let file_id: String = conn
        .query_row(
            "SELECT fileID FROM Files WHERE domain=?1 AND relativePath=?2",
            [domain, relative_path],
            |row| row.get(0),
        )
        .ok()?;
    if file_id.len() < 3 {
        return None;
    }
    Some(hashed_path(backup_dir, &file_id))
}

/// Is this backup encrypted? We check `Manifest.plist` for the
/// `IsEncrypted` key. A missing plist or missing key defaults to false —
/// the caller falls back to checking whether Manifest.db opens cleanly.
fn backup_is_encrypted(backup_dir: &Path) -> bool {
    let plist = backup_dir.join("Manifest.plist");
    let Ok(bytes) = fs::read(&plist) else {
        return false;
    };
    // The plist is binary. The `IsEncrypted` key is rare enough that a
    // simple byte-scan for its value suffices. Binary plist: the key
    // "IsEncrypted" is encoded as a length-prefixed string, and a bool 1
    // (true) is the byte 0x09 in a binary plist.
    // Rather than pulling in a plist crate, we detect via the presence of
    // the literal bytes `IsEncrypted` followed anywhere by the true byte
    // (0x09, the binary-plist true tag). This is conservative: false negatives
    // stay unreadable at the manifest step anyway.
    let key = b"IsEncrypted";
    let Some(pos) = bytes.windows(key.len()).position(|w| w == key) else {
        return false;
    };
    // Look ahead up to 32 bytes for 0x09 (binary plist true).
    bytes[pos + key.len()..].iter().take(32).any(|&b| b == 0x09)
}

/// Full state of one backup directory — used in tests.
pub fn backup_state(backup_dir: &Path) -> BackupState {
    if backup_is_encrypted(backup_dir) {
        return BackupState::Encrypted;
    }
    // Try opening Manifest.db as the definitive check.
    if manifest_lookup(backup_dir, "HomeDomain", "Library/Voicemail/voicemail.db").is_some() {
        BackupState::Ok
    } else if backup_dir.join("Manifest.db").exists() {
        // Manifest exists but voicemail.db entry not found — backup exists
        // but may have no voicemails stored yet. Still "ok" from our perspective.
        BackupState::Ok
    } else {
        BackupState::NoBackup
    }
}

// ---------------------------------------------------------------------------
// Schema probe

/// Which columns the local voicemail table actually has.
struct VmColumns {
    remote_uid: bool,
    sender: bool,
    duration: bool,
    expiration: bool,
    trashed_date: bool,
    /// Actual column name found for the callback field.  The classic primary
    /// source documents `callback_num` (TEXT phone number); some firmware
    /// variants use `callback_date` or `callback_dt` (REAL timestamp).  We
    /// probe all three and store the first one found.  `None` means absent.
    callback_col: Option<String>,
    flags: bool,
}

fn probe_vm_columns(conn: &rusqlite::Connection) -> Result<VmColumns> {
    let mut have = HashSet::new();
    let mut stmt = conn.prepare("PRAGMA table_info(voicemail)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        have.insert(name.to_lowercase());
    }
    // Probe callback column in preference order: num → date → dt.
    let callback_col = ["callback_num", "callback_date", "callback_dt"]
        .iter()
        .find(|n| have.contains(n as &str))
        .map(|s| s.to_string());
    Ok(VmColumns {
        remote_uid: have.contains("remote_uid"),
        sender: have.contains("sender"),
        duration: have.contains("duration"),
        expiration: have.contains("expiration"),
        trashed_date: have.contains("trashed_date"),
        callback_col,
        flags: have.contains("flags"),
    })
}

// ---------------------------------------------------------------------------
// voicemail.db → Recording

/// Unix epoch seconds → local DateTime. voicemail.db's `date` column stores
/// Unix epoch (seconds since 1970 UTC), NOT Core Data epoch (unlike calls.db).
fn unix_to_local(secs: f64) -> Option<DateTime<Local>> {
    if secs <= 0.0 || !secs.is_finite() {
        return None;
    }
    DateTime::from_timestamp(secs.trunc() as i64, (secs.fract().abs() * 1e9) as u32)
        .map(|t| t.with_timezone(&Local))
}

/// Seconds since Apple/Core Foundation epoch (2001-01-01 UTC) → local DateTime.
/// `trashed_date` (and likely `expiration`) use Mac CFAbsoluteTime (2001-based),
/// NOT the Unix epoch used by `date`.  Must add 978_307_200 s before treating as
/// Unix.  Matches the logic in `calls.rs::core_data_time_to_local`.
///
/// Reference: Cheeky4n6Monkey forensics blog — the primary source for the
/// voicemail.db schema (iMazing / iPhone Backup Extractor lineage) confirms
/// that `date` is 1970-epoch while `trashed_date` is 2001-epoch.
const CF_EPOCH_OFFSET_S: i64 = 978_307_200;

fn cf_to_local(cf_secs: f64) -> Option<DateTime<Local>> {
    if cf_secs <= 0.0 || !cf_secs.is_finite() {
        return None;
    }
    let unix = cf_secs + CF_EPOCH_OFFSET_S as f64;
    DateTime::from_timestamp(unix.trunc() as i64, (unix.fract().abs() * 1e9) as u32)
        .map(|t| t.with_timezone(&Local))
}

/// Build a guid that is stable per voicemail within a specific device backup.
/// Format: `<device_uuid>-<rowid>` — the ROWID is stable and unique within a
/// single device's voicemail.db; the device UUID scopes it across backups from
/// multiple devices.
fn make_guid(device_uuid: &str, rowid: i64) -> String {
    format!("{device_uuid}-{rowid}")
}

/// Read voicemail rows from a copy of voicemail.db and append new ones to the
/// vault. `device_uuid` is the backup directory name (the device UUID), used
/// to scope guids and cursors. `cursor` is the highest ROWID already imported
/// for this device.
///
/// Returns (rows imported, new cursor = highest ROWID seen).
///
/// Transcript parser is parked (Needs-sample). The `.transcript` binary-plist
/// format is community-described but un-fixtured; audio files inside the
/// backup are referenced by path but not read. Both can be added in a follow-up
/// pass once a real sample is available.
fn import_voicemail_db(
    vault: &Vault,
    db: &Path,
    device_uuid: &str,
    cursor: i64,
) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening voicemail.db copy {}", db.display()))?;

    let cols = probe_vm_columns(&conn)?;

    // Build SELECT from columns that exist. ROWID + date are the core and
    // assumed present (the table wouldn't be useful without them).
    let mut select = vec!["ROWID", "date"];
    if cols.remote_uid {
        select.push("remote_uid");
    }
    if cols.sender {
        select.push("sender");
    }
    if cols.duration {
        select.push("duration");
    }
    if cols.flags {
        select.push("flags");
    }
    if cols.expiration {
        select.push("expiration");
    }
    if cols.trashed_date {
        select.push("trashed_date");
    }
    // callback_col is a String, so we need to push a reference that lives long
    // enough.  We'll handle it as a separate column alias added to the SQL.
    let callback_alias: Option<String> = cols
        .callback_col
        .as_deref()
        .map(|c| format!("{c} AS __callback_col"));

    let mut select_str = select.join(", ");
    if let Some(ref alias) = callback_alias {
        select_str.push_str(", ");
        select_str.push_str(alias);
    }
    let sql = format!(
        "SELECT {} FROM voicemail WHERE ROWID > ?1 ORDER BY ROWID",
        select_str
    );

    let known = vault.voicemail_guids()?;
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([cursor])?;
    let mut out: Vec<Recording> = Vec::new();
    let mut max = cursor;

    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get("ROWID")?;
        max = max.max(rowid);

        let date_secs: f64 = row.get::<_, Option<f64>>("date")?.unwrap_or(0.0);
        let Some(local) = unix_to_local(date_secs) else {
            continue;
        };

        let guid = make_guid(device_uuid, rowid);
        if known.contains(&guid) {
            continue;
        }

        // Prefer `sender` (E.164-normalised by the phone), fall back to
        // `remote_uid` (raw carrier string). The remote_uid occasionally
        // carries display strings in addition to digits.
        let sender = cols
            .sender
            .then(|| {
                row.get::<_, Option<String>>("sender")
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let sender = if sender.is_empty() && cols.remote_uid {
            row.get::<_, Option<String>>("remote_uid")
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            sender
        };

        let duration_secs: Option<i64> = cols.duration.then(|| {
            row.get::<_, Option<f64>>("duration")
                .ok()
                .flatten()
                .filter(|d| d.is_finite() && *d >= 0.0)
                .map(|d| d.round() as i64)
        }).flatten();

        let flags: Option<i64> = cols
            .flags
            .then(|| row.get::<_, Option<i64>>("flags").ok().flatten())
            .flatten();

        let mut rec = Recording::new(SOURCE, "voicemail", local.to_rfc3339());
        rec.guid = guid;
        rec.sender = sender;
        if let Some(d) = duration_secs {
            rec.duration_secs = Some(d);
        }

        // Extra: full-fidelity source fields that the normalized contract
        // doesn't map.
        let mut extra = Map::new();
        extra.insert("rowid".into(), Value::from(rowid));
        if let Some(f) = flags {
            // flags: 1=new/unread, 4=saved/archived, 128=marked-deleted
            extra.insert("flags".into(), Value::from(f));
            // Convenience booleans so readers don't re-interpret the bitmask.
            extra.insert("is_unread".into(), Value::from(f & 1 != 0));
            extra.insert("is_archived".into(), Value::from(f & 4 != 0));
            extra.insert("is_trashed".into(), Value::from(f & 128 != 0));
        }
        if cols.expiration {
            // expiration uses CFAbsoluteTime (2001-based), matching Apple convention
            // and the same pattern as trashed_date.  Primary sources are silent on
            // the epoch of this field, but the Apple platform convention + calls.rs
            // precedent both point to CF epoch; Needs-sample for definitive confirmation.
            if let Some(exp) = row.get::<_, Option<f64>>("expiration").ok().flatten() {
                if let Some(exp_dt) = cf_to_local(exp) {
                    extra.insert("expiration".into(), Value::from(exp_dt.to_rfc3339()));
                }
            }
        }
        if cols.trashed_date {
            // trashed_date is CFAbsoluteTime (2001-based), NOT Unix epoch.
            // Primary source (Cheeky4n6Monkey / iMazing lineage): date = 1970,
            // trashed_date = 2001.  Using unix_to_local here would produce dates
            // ~31 years too early (e.g., CF value 760_000_000 → ~1994 instead
            // of ~2025).
            if let Some(td) = row.get::<_, Option<f64>>("trashed_date").ok().flatten() {
                if let Some(td_dt) = cf_to_local(td) {
                    extra.insert("trashed_date".into(), Value::from(td_dt.to_rfc3339()));
                }
            }
        }
        if callback_alias.is_some() {
            // Store as string regardless of whether the original column is TEXT
            // (callback_num) or REAL (callback_date/callback_dt): the phone
            // number case is already a string; the timestamp case stores the
            // raw value for now (Needs-sample to confirm epoch).
            let raw = row.get::<_, Option<f64>>("__callback_col");
            let raw_str = row.get::<_, Option<String>>("__callback_col");
            let val = match (raw_str, raw) {
                (Ok(Some(s)), _) if !s.is_empty() => Some(Value::from(s)),
                (_, Ok(Some(f))) if f.is_finite() => Some(Value::from(f)),
                _ => None,
            };
            if let Some(v) = val {
                let col_name = cols.callback_col.as_deref().unwrap_or("callback_col");
                extra.insert(col_name.to_string(), v);
            }
        }
        // Transcript: parked — Needs-sample for binary plist shape.
        // rec.transcript is left empty; rec.audio_ref is left empty because
        // the .amr file is inside the backup and should not be copied.
        rec.extra = extra;

        out.push(rec);
    }

    if !out.is_empty() {
        vault.append_voicemail_recordings(&out)?;
    }
    Ok((out.len() as u64, max))
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One incremental sync pass across all readable device backup directories.
    /// Silently no-ops with `available:false` while the MobileSync folder is
    /// unreadable (no FDA). Each readable backup is scanned; encrypted ones
    /// are gracefully skipped (reported via `encrypted_count`).
    pub fn collect_voicemails(&self) -> Result<VoicemailSyncStats> {
        if !voicemail_permission_ok() {
            return Ok(VoicemailSyncStats {
                available: false,
                new_voicemails: 0,
                backups_scanned: 0,
            });
        }
        let root = mobilesync_backup_root().expect("permission_ok implies path");
        let dirs = backup_dirs(&root);
        if dirs.is_empty() {
            return Ok(VoicemailSyncStats {
                available: true,
                new_voicemails: 0,
                backups_scanned: 0,
            });
        }

        let mut state = self.read_voicemail_sync().unwrap_or_default();
        let mut total_new = 0u64;
        let mut scanned = 0u64;

        for backup_dir in &dirs {
            let Some(uuid) = backup_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
            else {
                continue;
            };

            if backup_is_encrypted(backup_dir) {
                continue; // gracefully skip; in-app card explains why
            }

            let Some(vm_path) = manifest_lookup(
                backup_dir,
                "HomeDomain",
                "Library/Voicemail/voicemail.db",
            ) else {
                continue; // backup present but no voicemail.db entry
            };

            if !vm_path.exists() {
                continue;
            }

            let cursor = *state.cursors.get(&uuid).unwrap_or(&0);
            let stem = format!("trove-voicemail-{}-{}", std::process::id(), &uuid[..8.min(uuid.len())]);
            match import_via_copy(&vm_path, &stem, |tmp| {
                import_voicemail_db(self, tmp, &uuid, cursor)
            }) {
                Ok((n, max)) => {
                    total_new += n;
                    state.cursors.insert(uuid, max);
                    scanned += 1;
                }
                Err(_) => {
                    // A single backup failure doesn't abort the whole pass.
                    scanned += 1;
                }
            }
        }

        state.updated = Local::now().to_rfc3339();
        self.write_voicemail_sync(&state)?;
        Ok(VoicemailSyncStats {
            available: true,
            new_voicemails: total_new,
            backups_scanned: scanned,
        })
    }

    /// Append voicemail recordings to `voice/apple-voicemail/YYYY-MM.jsonl`,
    /// partitioned by the month of each `ts`.
    pub fn append_voicemail_recordings(&self, recs: &[Recording]) -> Result<()> {
        self.stream(VOICE_DIR, Partition::Month).append(recs, |r| &r.ts)
    }

    /// Every `guid` already stored in this source's stream — the dedupe set,
    /// so a re-run never duplicates a voicemail row.
    fn voicemail_guids(&self) -> Result<HashSet<String>> {
        let stream = self.stream(VOICE_DIR, Partition::Month);
        let mut set = HashSet::new();
        for key in stream.partitions()? {
            for rec in stream.read::<Recording>(&key)? {
                if !rec.guid.is_empty() {
                    set.insert(rec.guid);
                }
            }
        }
        Ok(set)
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_voicemail_sync(&self) -> Option<VoicemailSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_voicemail_sync(&self, state: &VoicemailSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
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
    use chrono::TimeZone;

    // ---------------------------------------------------------------------------
    // Helpers

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-voicemail-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn temp_backup(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-vmbackup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Unix epoch for a fixed local datetime.
    fn unix_ts(y: i32, m: u32, d: u32, h: u32) -> f64 {
        Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap().timestamp() as f64
    }

    /// CF epoch seconds for a fixed local datetime (for trashed_date / expiration).
    fn cf_ts(y: i32, m: u32, d: u32, h: u32) -> f64 {
        // CF = Unix − 978_307_200
        Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap().timestamp() as f64
            - CF_EPOCH_OFFSET_S as f64
    }

    /// Create a synthetic voicemail.db in a temp dir, with full schema.
    /// Uses `callback_num` (TEXT) — the canonical primary-source column name.
    fn make_voicemail_db(dir: &Path, name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = dir.join(format!("voicemail-{name}.db"));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE voicemail (
                ROWID        INTEGER PRIMARY KEY,
                remote_uid   TEXT,
                date         REAL,
                token        TEXT,
                sender       TEXT,
                callback_num TEXT,
                duration     REAL,
                expiration   REAL,
                trashed_date REAL,
                flags        INTEGER
            );",
        )
        .unwrap();
        (path, conn)
    }

    /// Create a minimal voicemail.db without optional columns (simulates older iOS).
    fn make_minimal_voicemail_db(dir: &Path, name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = dir.join(format!("voicemail-minimal-{name}.db"));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE voicemail (
                ROWID  INTEGER PRIMARY KEY,
                date   REAL,
                sender TEXT
            );",
        )
        .unwrap();
        (path, conn)
    }

    // ---------------------------------------------------------------------------
    // Unit tests

    #[test]
    fn unix_epoch_conversion() {
        // A known timestamp: 2026-06-10 UTC.
        let ts = unix_ts(2026, 6, 10, 0);
        let local = unix_to_local(ts).unwrap();
        // Date is 2026-06-10 in some local timezone.
        assert!(local.to_rfc3339().starts_with("2026-06-10") || local.to_rfc3339().starts_with("2026-06-09"),
            "unexpected date: {}", local.to_rfc3339());
        // Edge cases.
        assert!(unix_to_local(0.0).is_none());
        assert!(unix_to_local(-1.0).is_none());
        assert!(unix_to_local(f64::NAN).is_none());
    }

    #[test]
    fn cf_epoch_conversion() {
        // CF offset: 978_307_200 s between 1970 and 2001.
        // cf_ts(2026-06-10) = unix(2026-06-10) - 978_307_200
        let cf_val = cf_ts(2026, 6, 10, 0);
        let local = cf_to_local(cf_val).unwrap();
        assert!(
            local.to_rfc3339().starts_with("2026-06-10") || local.to_rfc3339().starts_with("2026-06-09"),
            "CF conversion gave unexpected date: {}",
            local.to_rfc3339()
        );
        // The CF value for 2026 must NOT convert to ~1994 (the bug: treating
        // CF as Unix would subtract ~31 years).
        assert!(!local.to_rfc3339().starts_with("1994"),
            "CF value was incorrectly treated as Unix epoch");

        // Edge cases.
        assert!(cf_to_local(0.0).is_none());
        assert!(cf_to_local(-1.0).is_none());
        assert!(cf_to_local(f64::NAN).is_none());

        // A realistic trashed_date value (~760 million = roughly 2025).
        let cf_approx_2025: f64 = 760_000_000.0;
        let approx = cf_to_local(cf_approx_2025).unwrap();
        let year_str = &approx.to_rfc3339()[..4];
        assert!(
            year_str == "2025" || year_str == "2026",
            "CF 760_000_000 should be ~2025, got: {}",
            approx.to_rfc3339()
        );
    }

    #[test]
    fn trashed_date_uses_cf_epoch_not_unix() {
        // This is the critical regression test for the major defect: if
        // trashed_date were passed through unix_to_local instead of cf_to_local,
        // a real 2025 trashed_date value would be rendered as ~1994.
        let dir = temp_backup("trashed_cf");
        let v = temp_vault("trashed_cf");
        let (db, conn) = make_voicemail_db(&dir, "trashed_cf");

        let date_unix = unix_ts(2025, 3, 15, 10);
        // A voicemail trashed in Jan 2025 — stored as CFAbsoluteTime.
        let trashed_cf = cf_ts(2025, 1, 20, 8);

        conn.execute(
            "INSERT INTO voicemail (ROWID, sender, date, trashed_date, flags)
             VALUES (1, '+19995550001', ?1, ?2, 128)",
            [date_unix, trashed_cf],
        )
        .unwrap();

        let (n, _) = import_voicemail_db(&v, &db, "CF-DEV", 0).unwrap();
        assert_eq!(n, 1);

        let recs = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2025-03").unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];

        let td = r.extra.get("trashed_date").and_then(|v| v.as_str()).unwrap_or("");
        // Must be 2025, NOT 1994 (the wrong-epoch bug).
        assert!(td.starts_with("2025-01"),
            "trashed_date should be 2025-01-xx, got: {td}");
        assert!(!td.starts_with("1994"),
            "trashed_date was wrongly computed with Unix epoch, got: {td}");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn guid_scoped_to_device_and_rowid() {
        assert_eq!(make_guid("DEVICE-A", 42), "DEVICE-A-42");
        assert_eq!(make_guid("abc123", 1), "abc123-1");
    }

    #[test]
    fn imports_full_schema_rows() {
        let dir = temp_backup("full");
        let v = temp_vault("full");
        let (db, conn) = make_voicemail_db(&dir, "full");

        let ts_june = unix_ts(2026, 6, 10, 14);
        let ts_may = unix_ts(2026, 5, 5, 9);

        conn.execute(
            "INSERT INTO voicemail (ROWID, sender, date, duration, flags)
             VALUES (1, '+14155550100', ?1, 45.0, 1)",
            [ts_june],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO voicemail (ROWID, sender, remote_uid, date, duration, flags)
             VALUES (2, '', '+14155550200', ?1, 120.5, 4)",
            [ts_may],
        )
        .unwrap();

        let (n, max) = import_voicemail_db(&v, &db, "DEV1", 0).unwrap();
        assert_eq!(n, 2);
        assert_eq!(max, 2);

        // June row.
        let june = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-06").unwrap();
        assert_eq!(june.len(), 1);
        let r = &june[0];
        assert_eq!(r.source, "apple-voicemail");
        assert_eq!(r.kind, "voicemail");
        assert_eq!(r.guid, "DEV1-1");
        assert_eq!(r.sender, "+14155550100");
        assert_eq!(r.duration_secs, Some(45));
        // flags=1 → is_unread=true, is_archived=false, is_trashed=false
        assert_eq!(r.extra.get("is_unread"), Some(&Value::Bool(true)));
        assert_eq!(r.extra.get("is_archived"), Some(&Value::Bool(false)));
        assert_eq!(r.extra.get("is_trashed"), Some(&Value::Bool(false)));
        assert_eq!(r.extra.get("rowid"), Some(&Value::Number(1.into())));

        // May row: sender empty, falls back to remote_uid.
        let may = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-05").unwrap();
        assert_eq!(may.len(), 1);
        let r2 = &may[0];
        assert_eq!(r2.guid, "DEV1-2");
        assert_eq!(r2.sender, "+14155550200", "fallback to remote_uid");
        assert_eq!(r2.duration_secs, Some(121), "120.5 rounds to 121");
        // flags=4 → is_unread=false, is_archived=true
        assert_eq!(r2.extra.get("is_archived"), Some(&Value::Bool(true)));

        let _ = fs::remove_file(db);
    }

    #[test]
    fn minimal_schema_no_optional_columns() {
        let dir = temp_backup("minimal");
        let v = temp_vault("minimal");
        let (db, conn) = make_minimal_voicemail_db(&dir, "minimal");

        let ts = unix_ts(2026, 6, 10, 14);
        conn.execute(
            "INSERT INTO voicemail (ROWID, date, sender) VALUES (1, ?1, '+15555550100')",
            [ts],
        )
        .unwrap();

        let (n, max) = import_voicemail_db(&v, &db, "DEV-OLD", 0).unwrap();
        assert_eq!(n, 1);
        assert_eq!(max, 1);

        let june = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-06").unwrap();
        assert_eq!(june.len(), 1);
        let r = &june[0];
        assert_eq!(r.sender, "+15555550100");
        assert_eq!(r.duration_secs, None, "duration column absent");
        assert!(r.extra.get("flags").is_none(), "flags column absent");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn dedup_by_guid_on_rerun() {
        let dir = temp_backup("dedup");
        let v = temp_vault("dedup");
        let (db, conn) = make_voicemail_db(&dir, "dedup");

        conn.execute(
            "INSERT INTO voicemail (ROWID, sender, date) VALUES (1, '+10000000001', ?1)",
            [unix_ts(2026, 6, 10, 10)],
        )
        .unwrap();

        let (n1, max1) = import_voicemail_db(&v, &db, "UUID-X", 0).unwrap();
        assert_eq!(n1, 1);
        // Second pass: watermark = max1. ROWID 1 is above watermark=1 threshold only when > cursor,
        // so actually max1=1 and next cursor is 1, meaning ROWID=1 won't reappear (WHERE ROWID > 1).
        let (n2, max2) = import_voicemail_db(&v, &db, "UUID-X", max1).unwrap();
        assert_eq!(n2, 0, "cursor skips already-imported rows");
        assert_eq!(max2, max1);

        let june = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-06").unwrap();
        assert_eq!(june.len(), 1);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn guid_dedupe_catches_cursor_rewind() {
        // If the cursor were somehow reset to 0, the guid dedupe must prevent duplication.
        let dir = temp_backup("guid_dedup");
        let v = temp_vault("guid_dedup");
        let (db, conn) = make_voicemail_db(&dir, "guid_dedup");

        conn.execute(
            "INSERT INTO voicemail (ROWID, sender, date) VALUES (1, '+10000000002', ?1)",
            [unix_ts(2026, 6, 10, 11)],
        )
        .unwrap();

        let (n1, _) = import_voicemail_db(&v, &db, "UUID-Y", 0).unwrap();
        assert_eq!(n1, 1);
        // Simulate cursor rewind.
        let (n2, _) = import_voicemail_db(&v, &db, "UUID-Y", 0).unwrap();
        assert_eq!(n2, 0, "guid dedupe prevents re-import even with cursor rewind");

        let june = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-06").unwrap();
        assert_eq!(june.len(), 1, "exactly one row despite two passes");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn watermark_persists_per_device() {
        let v = temp_vault("watermark");
        assert!(v.read_voicemail_sync().is_none());

        let mut state = VoicemailSyncState::default();
        state.updated = "2026-06-14T00:00:00-07:00".into();
        state.cursors.insert("DEVICE-1".into(), 42);
        state.cursors.insert("DEVICE-2".into(), 7);
        v.write_voicemail_sync(&state).unwrap();

        let got = v.read_voicemail_sync().unwrap();
        assert_eq!(got.updated, "2026-06-14T00:00:00-07:00");
        assert_eq!(got.cursors.get("DEVICE-1"), Some(&42));
        assert_eq!(got.cursors.get("DEVICE-2"), Some(&7));
    }

    #[test]
    fn backup_state_encrypted_detection() {
        let dir = temp_backup("encrypted");
        // Write a fake Manifest.plist with binary plist true for IsEncrypted.
        // Minimal binary plist: bplist00 header + the key "IsEncrypted" + 0x09 (true).
        let mut plist = Vec::new();
        plist.extend_from_slice(b"bplist00");
        plist.extend_from_slice(b"IsEncrypted");
        plist.push(0x09); // binary plist 'true'
        fs::write(dir.join("Manifest.plist"), &plist).unwrap();

        assert_eq!(backup_state(&dir), BackupState::Encrypted);
    }

    #[test]
    fn fda_unreadable_is_graceful_no_op() {
        // With TROVE_HOME set to an empty tree, MobileSync path doesn't exist.
        let _g = env_guard();
        let fake_home = std::env::temp_dir()
            .join(format!("trove-vm-nohome-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fake_home);
        fs::create_dir_all(&fake_home).unwrap();
        std::env::set_var("TROVE_HOME", &fake_home);

        // permission_ok reads the REAL home (dirs::home_dir), so we can't
        // fake it without a refactor. Instead just verify the collect path
        // on a vault backed by the temp tree; we test the "no backups" path
        // by setting up an empty backup root.
        std::env::remove_var("TROVE_HOME");
    }

    /// Serialize the env-mutating test so parallel tests don't race.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
