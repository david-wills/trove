//! macOS Download History via the QuarantineEventsV2 SQLite database.
//!
//! The OS quarantine system logs every file downloaded by Safari, Chrome,
//! Firefox, Messages (AirDrop/iMessage attachments), and any
//! quarantine-aware app into a single home-dir SQLite database:
//! `~/Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2`.
//!
//! Records carry the **source URL and referrer**, the downloading app
//! (bundle id + name), sender info (Mail/AirDrop), and persist even after
//! the downloaded file is deleted.  No permissions prompt — the Preferences
//! directory is readable without FDA.  Copy-then-open; never write.
//!
//! Writes raw-only to `files/macos-downloads/YYYY-MM.jsonl` (one row per
//! download event) with an incremental cursor on `LSQuarantineTimeStamp`
//! (Apple epoch: seconds since 2001-01-01 UTC).
//!
//! Brief: docs/integrations/macos-downloads.md.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Seconds between the Unix epoch (1970-01-01) and the Apple/Core Data epoch
/// (2001-01-01). `LSQuarantineTimeStamp` is REAL seconds since 2001, UTC.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

/// Seconds between quarantine scans (hourly — the DB is low-volume).
const SYNC_SECS: u64 = 3_600;

const SYNC_FILE: &str = ".trove/macos-downloads-sync.json";
const VAULT_DIR: &str = "files/macos-downloads";

// ---------------------------------------------------------------------------
// Public types

/// One quarantine (download) event, written to `files/macos-downloads/YYYY-MM.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadEvent {
    /// RFC3339 local time of the download.
    pub ts: String,
    /// `LSQuarantineEventIdentifier` UUID — stable, immutable dedup key.
    pub guid: String,
    /// Source URL (the file's direct URL), if recorded by the agent.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Referring / origin page URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub referrer_url: String,
    /// Title of the origin page.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin_title: String,
    /// Bundle ID of the app that triggered the download (e.g. `com.google.Chrome`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app_bundle_id: String,
    /// Human-readable app name (e.g. `Chrome`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app_name: String,
    /// Sender name (Mail / AirDrop only; usually empty for browser downloads).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sender_name: String,
    /// Sender address (Mail / AirDrop only).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sender_address: String,
    /// `LSQuarantineTypeNumber` — 0 = web download, 3 = attachment/AirDrop.
    /// Always serialized (0 is a real category, not "absent").
    pub type_number: u32,
}

/// Persisted sync state, stored at `.trove/macos-downloads-sync.json`.
/// Rebuildable from the JSONL logs by scanning for the highest `ts`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest imported `LSQuarantineTimeStamp` (Apple-epoch seconds, as
    /// stored — REAL but compared exactly as f64 bit pattern).
    pub cursor: f64,
}

// ---------------------------------------------------------------------------
// DEF hooks

fn quarantine_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2")
    })
}

/// Whether this process can open the quarantine database.
pub fn macos_downloads_permission_ok() -> bool {
    quarantine_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "file-readable",
        granted: Some(macos_downloads_permission_ok()),
        required: false,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_download_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_macos_downloads()?;
    Ok(CollectOutcome::note_if(s.new_events > 0, || {
        format!("imported {} download events", s.new_events)
    }))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "macos-downloads",
        name: "macOS Downloads",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Records files you have downloaded on this Mac — file \
                      name, source URL, referrer, and download timestamp — \
                      from the macOS quarantine database. Works with Safari, \
                      Chrome, Firefox, Messages, and any quarantine-aware app.",
        domain: "files",
        vault_path: "files/macos-downloads/",
        toggleable: true,
        setup: &["Works immediately — no permissions or configuration needed."],
        caveats: "Records survive file deletion (the vault is the durable \
                  copy). Users can clear the source table via Finder's \
                  \"Clear Downloads\" — already-synced rows remain in the \
                  vault.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Result type

/// Result of one sync pass.
#[derive(Debug, Clone)]
pub struct DownloadSyncStats {
    /// False when the DB is unreadable.
    pub available: bool,
    pub new_events: u64,
}

// ---------------------------------------------------------------------------
// Core import logic (split from the copy step so tests can run on a
// synthetic DB)

/// Apple-epoch REAL seconds → RFC3339 local. None for zero/negative values.
fn apple_secs_to_local(secs: f64) -> Option<DateTime<Local>> {
    if secs <= 0.0 {
        return None;
    }
    let unix_secs = secs as i64 + APPLE_EPOCH_OFFSET_S;
    let frac = secs.fract();
    let nanos = (frac * 1_000_000_000.0) as u32;
    DateTime::from_timestamp(unix_secs, nanos).map(|t| t.with_timezone(&Local))
}

/// RFC3339 string → Apple-epoch REAL seconds (inverse of [`apple_secs_to_local`]).
/// Used by [`Vault::rebuild_download_sync`] to reconstruct the cursor from JSONL.
fn rfc3339_to_apple_secs(ts: &str) -> Option<f64> {
    let dt = DateTime::parse_from_rfc3339(ts).ok()?;
    let unix_secs = dt.timestamp();
    let nanos = dt.timestamp_subsec_nanos();
    let apple = (unix_secs - APPLE_EPOCH_OFFSET_S) as f64 + (nanos as f64 / 1_000_000_000.0);
    if apple <= 0.0 {
        None
    } else {
        Some(apple)
    }
}

/// Read events with `LSQuarantineTimeStamp > cursor` out of a DB (or copy)
/// and append them to the vault. Returns (count, new cursor).
pub(crate) fn import_quarantine_db(
    vault: &Vault,
    db: &std::path::Path,
    cursor: f64,
) -> Result<(u64, f64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening quarantine DB copy {}", db.display()))?;

    let mut stmt = conn.prepare(
        "SELECT LSQuarantineEventIdentifier,
                LSQuarantineTimeStamp,
                COALESCE(LSQuarantineAgentBundleIdentifier, ''),
                COALESCE(LSQuarantineAgentName, ''),
                COALESCE(LSQuarantineDataURLString, ''),
                COALESCE(LSQuarantineSenderName, ''),
                COALESCE(LSQuarantineSenderAddress, ''),
                COALESCE(LSQuarantineTypeNumber, 0),
                COALESCE(LSQuarantineOriginTitle, ''),
                COALESCE(LSQuarantineOriginURLString, '')
         FROM LSQuarantineEvent
         WHERE LSQuarantineTimeStamp > ?1
         ORDER BY LSQuarantineTimeStamp",
    )?;

    let mut rows = stmt.query([cursor])?;
    let mut events: Vec<DownloadEvent> = Vec::new();
    let mut max = cursor;

    while let Some(row) = rows.next()? {
        let guid: String = row.get(0)?;
        let ts_secs: f64 = row.get(1)?;
        max = max.max(ts_secs);

        let Some(local) = apple_secs_to_local(ts_secs) else {
            continue;
        };

        events.push(DownloadEvent {
            ts: local.to_rfc3339(),
            guid,
            url: row.get(4)?,
            referrer_url: row.get(9)?,
            origin_title: row.get(8)?,
            app_bundle_id: row.get(2)?,
            app_name: row.get(3)?,
            sender_name: row.get(5)?,
            sender_address: row.get(6)?,
            type_number: row.get(7)?,
        });
    }

    if !events.is_empty() {
        vault
            .stream(VAULT_DIR, Partition::Month)
            .append(&events, |e| &e.ts)?;
    }

    Ok((events.len() as u64, max))
}

// ---------------------------------------------------------------------------
// Vault methods

impl Vault {
    /// One incremental sync pass over the quarantine database.
    pub fn collect_macos_downloads(&self) -> Result<DownloadSyncStats> {
        if !macos_downloads_permission_ok() {
            return Ok(DownloadSyncStats {
                available: false,
                new_events: 0,
            });
        }
        let db = quarantine_db_path().expect("permission_ok implies path");
        let mut state = match self.read_download_sync() {
            Some(s) => s,
            None => self.rebuild_download_sync(),
        };

        let stem = format!("trove-macos-downloads-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| {
            import_quarantine_db(self, tmp, state.cursor)
        })?;

        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_download_sync(&state)?;

        Ok(DownloadSyncStats {
            available: true,
            new_events: n,
        })
    }

    /// Read the persisted sync state (if present).
    pub fn read_download_sync(&self) -> Option<DownloadSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_download_sync(&self, state: &DownloadSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }

    /// Reconstruct the sync cursor from JSONL logs — used when the sync file
    /// is missing so a re-sync appends only genuinely new events instead of
    /// re-importing every quarantine row from scratch (which would append
    /// duplicates, since the store's append does not guid-dedupe on disk).
    ///
    /// Scans `files/macos-downloads/*.jsonl` for the highest `ts` value and
    /// converts it back to Apple-epoch seconds.
    fn rebuild_download_sync(&self) -> DownloadSyncState {
        let dir = self.root().join(VAULT_DIR);
        let Ok(entries) = fs::read_dir(&dir) else {
            return DownloadSyncState::default();
        };
        let mut max_apple: f64 = 0.0;
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            for line in body.lines() {
                let Ok(ev) = serde_json::from_str::<DownloadEvent>(line) else {
                    continue;
                };
                // Convert the RFC3339 ts back to Apple-epoch seconds.
                if let Some(apple) = rfc3339_to_apple_secs(&ev.ts) {
                    if apple > max_apple {
                        max_apple = apple;
                    }
                }
            }
        }
        DownloadSyncState {
            updated: String::new(),
            cursor: max_apple,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-macos-dl-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn fake_quarantine_db(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir().join(format!(
            "trove-fake-quarantine-{}-{name}.db",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE LSQuarantineEvent (
                LSQuarantineEventIdentifier TEXT NOT NULL PRIMARY KEY,
                LSQuarantineTimeStamp REAL,
                LSQuarantineAgentBundleIdentifier TEXT,
                LSQuarantineAgentName TEXT,
                LSQuarantineDataURLString TEXT,
                LSQuarantineSenderName TEXT,
                LSQuarantineSenderAddress TEXT,
                LSQuarantineTypeNumber INTEGER,
                LSQuarantineOriginTitle TEXT,
                LSQuarantineOriginURLString TEXT,
                LSQuarantineOriginAlias BLOB
            );",
        )
        .unwrap();
        (path, conn)
    }

    /// Convert a Unix timestamp to Apple-epoch seconds (seconds since 2001-01-01).
    fn apple_ts(unix_secs: i64) -> f64 {
        (unix_secs - APPLE_EPOCH_OFFSET_S) as f64
    }

    #[test]
    fn epoch_conversion_round_trips() {
        // 2025-06-16T21:44:04Z → Unix 1750110244.
        let unix: i64 = 1_750_110_244;
        let apple = apple_ts(unix);
        let local = apple_secs_to_local(apple).unwrap();
        assert_eq!(local.timestamp(), unix);

        // Fractional seconds preserved.
        let with_frac = apple + 0.5;
        let local2 = apple_secs_to_local(with_frac).unwrap();
        assert_eq!(local2.timestamp(), unix);
        assert!(local2.timestamp_subsec_millis() >= 499); // 0.5s → ≥499 ms

        // Zero/negative → None.
        assert!(apple_secs_to_local(0.0).is_none());
        assert!(apple_secs_to_local(-1.0).is_none());
    }

    #[test]
    fn import_is_incremental_and_dedupes() {
        let v = temp_vault("incremental");
        let (db, conn) = fake_quarantine_db("incremental");

        // Timestamps in 2026-06 (Apple epoch = Unix - 978307200).
        // 2026-06-10T12:00:00Z → unix 1781092800 → apple 802785600.
        let t1 = 802_785_600_f64; // 2026-06-10
        let t2 = 802_785_700_f64; // 2026-06-10 + 100s
        let t3 = 802_785_800_f64; // 2026-06-10 + 200s

        // Chrome browser download (browser row — sparse fields).
        conn.execute(
            "INSERT INTO LSQuarantineEvent VALUES (
                'GUID-CHROME-1', ?1,
                'com.google.Chrome', 'Chrome',
                'https://example.com/file.zip', '', '', 0,
                'Example Downloads', 'https://example.com/downloads',
                NULL
            )",
            [t1],
        )
        .unwrap();

        // Messages/AirDrop attachment (type_number=3, sender fields populated).
        conn.execute(
            "INSERT INTO LSQuarantineEvent VALUES (
                'GUID-MSG-1', ?1,
                'com.apple.MobileSMS', 'Messages',
                '', 'Alice Smith', 'alice@example.com', 3,
                '', '', NULL
            )",
            [t2],
        )
        .unwrap();

        // Sparse row — no URL, no sender.
        conn.execute(
            "INSERT INTO LSQuarantineEvent VALUES (
                'GUID-SPARSE-1', ?1,
                'com.apple.sharingd', 'sharingd',
                '', '', '', 3,
                '', '', NULL
            )",
            [t3],
        )
        .unwrap();

        let (n, cursor) = import_quarantine_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 3, "all three rows imported");
        assert!(cursor >= t3, "cursor advanced to max timestamp");

        // Read back from vault.
        let stream = v.stream(VAULT_DIR, Partition::Month);
        let month = "2026-06"; // all events in June 2026
        let events: Vec<DownloadEvent> = stream.read(month).unwrap();
        assert_eq!(events.len(), 3);

        // Chrome row has url and referrer.
        let chrome = events.iter().find(|e| e.guid == "GUID-CHROME-1").unwrap();
        assert_eq!(chrome.app_name, "Chrome");
        assert_eq!(chrome.url, "https://example.com/file.zip");
        assert_eq!(chrome.referrer_url, "https://example.com/downloads");
        // type_number=0 (web download) is always serialized — 0 is a real category.
        assert_eq!(chrome.type_number, 0);

        // Messages row has sender fields.
        let msg = events.iter().find(|e| e.guid == "GUID-MSG-1").unwrap();
        assert_eq!(msg.app_name, "Messages");
        assert_eq!(msg.sender_name, "Alice Smith");
        assert_eq!(msg.sender_address, "alice@example.com");
        assert_eq!(msg.type_number, 3);
        assert!(msg.url.is_empty());

        // Sparse row has empty optional fields (deserialized as empty strings).
        let sparse = events.iter().find(|e| e.guid == "GUID-SPARSE-1").unwrap();
        assert!(sparse.url.is_empty());
        assert!(sparse.sender_name.is_empty());

        // Second pass with same cursor: nothing new, cursor unchanged.
        let (n2, cursor2) = import_quarantine_db(&v, &db, cursor).unwrap();
        assert_eq!(n2, 0, "no duplicates on re-import");
        assert!((cursor2 - cursor).abs() < 1.0, "cursor unchanged");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn sync_state_persists_and_reloads() {
        let v = temp_vault("state");
        assert!(v.read_download_sync().is_none(), "no state before first sync");

        let state = DownloadSyncState {
            updated: "2026-06-16T21:00:00-07:00".into(),
            cursor: 803_000_000.0,
        };
        // Write via private method through collect (use write directly).
        let path = v.resolve(SYNC_FILE).unwrap();
        crate::store::write_json_atomic(&path, &state).unwrap();

        let loaded = v.read_download_sync().unwrap();
        assert_eq!(loaded.updated, "2026-06-16T21:00:00-07:00");
        assert!((loaded.cursor - 803_000_000.0).abs() < 1.0);
    }

    #[test]
    fn invalid_timestamp_rows_are_skipped() {
        let v = temp_vault("invalid_ts");
        let (db, conn) = fake_quarantine_db("invalid_ts");

        // Row with timestamp = 0 (invalid — should be skipped).
        conn.execute(
            "INSERT INTO LSQuarantineEvent VALUES (
                'GUID-ZERO-TS', 0.0,
                'com.google.Chrome', 'Chrome',
                'https://example.com/bad.zip', '', '', 0, '', '', NULL
            )",
            [],
        )
        .unwrap();

        // Valid row in 2026-06 (apple epoch 802785900 = 2026-06-10T12:05:00Z).
        let t_valid = 802_785_900_f64;
        conn.execute(
            "INSERT INTO LSQuarantineEvent VALUES (
                'GUID-VALID-1', ?1,
                'com.apple.Safari', 'Safari',
                'https://example.com/good.zip', '', '', 0, 'Example', 'https://example.com',
                NULL
            )",
            [t_valid],
        )
        .unwrap();

        let (n, _cursor) = import_quarantine_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1, "zero-timestamp row skipped, valid row imported");

        let events: Vec<DownloadEvent> =
            v.stream(VAULT_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].guid, "GUID-VALID-1");
        assert_eq!(events[0].app_name, "Safari");

        let _ = fs::remove_file(db);
    }

    /// When the sync file is lost, rebuild_download_sync() must reconstruct the
    /// cursor from the JSONL logs so the next collect() does not re-import every
    /// existing row (which would append duplicates — the store does not dedupe).
    #[test]
    fn cursor_rebuilds_from_jsonl_logs() {
        let v = temp_vault("rebuild");
        let (db, conn) = fake_quarantine_db("rebuild");

        // 2026-06-10T12:00:00Z and +300s
        let t1 = 802_785_600_f64;
        let t2 = 802_785_900_f64; // newest

        conn.execute(
            "INSERT INTO LSQuarantineEvent VALUES (
                'GUID-R1', ?1,
                'com.apple.Safari', 'Safari',
                'https://a.example.com/1.zip', '', '', 0, '', '', NULL
            )",
            [t1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO LSQuarantineEvent VALUES (
                'GUID-R2', ?1,
                'com.apple.Safari', 'Safari',
                'https://a.example.com/2.zip', '', '', 0, '', '', NULL
            )",
            [t2],
        )
        .unwrap();

        // First sync: imports both rows.
        let (n, cursor) = import_quarantine_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 2);
        assert!((cursor - t2).abs() < 1.0);

        // Simulate what collect_macos_downloads would write, then delete it
        // to mimic a lost/corrupt sync file.
        let sync_path = v.resolve(SYNC_FILE).unwrap();
        crate::store::write_json_atomic(
            &sync_path,
            &DownloadSyncState {
                updated: Local::now().to_rfc3339(),
                cursor,
            },
        )
        .unwrap();
        fs::remove_file(&sync_path).unwrap();
        assert!(v.read_download_sync().is_none(), "sync file gone");

        let rebuilt = v.rebuild_download_sync();
        // Rebuilt cursor should be within 1 second of the original Apple-epoch value.
        assert!(
            (rebuilt.cursor - t2).abs() < 1.0,
            "rebuilt cursor {:.1} should be near original {:.1}",
            rebuilt.cursor,
            t2
        );

        // With the rebuilt cursor, a second import should find nothing new.
        let (n2, _) = import_quarantine_db(&v, &db, rebuilt.cursor).unwrap();
        assert_eq!(n2, 0, "no duplicate imports after cursor rebuild");

        let _ = fs::remove_file(db);
    }
}
