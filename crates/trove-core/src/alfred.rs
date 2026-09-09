//! Alfred Clipboard — periodic local sync of Alfred's Powerpack clipboard history.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/alfred.md.
//!
//! A **Periodic** local-file collector that reads Alfred's SQLite clipboard store
//! and writes every clip (text, image metadata, file metadata) to the raw
//! `developer/alfred/YYYY-MM.jsonl` vault partitions.
//!
//! **`developer/` is raw-only** (taxonomy decision): no domain contract, no
//! normalized struct — this module owns its row shape.
//!
//! ## Database
//!
//! `~/Library/Application Support/Alfred/Databases/clipboard.alfdb`
//! Table: `clipboard(item, ts decimal, app, apppath, dataType integer, dataHash)`
//! Schema confirmed against a real Alfred Powerpack installation (2026-06-16).
//! dataType: 0 = text, 2 = image, 8 = file list.
//!
//! **Copy-then-open** (imessage/browser pattern): Alfred may hold the file open,
//! so we copy to a temp path before opening. No FDA prompt — home-dir path is
//! covered by troved's existing TCC grant.
//!
//! ## Privacy gate
//!
//! Clipboard history is uniquely sensitive (passwords, tokens, private text).
//! This integration is:
//! - **Default off** — opt-in with explicit acknowledgement.
//! - **App-name exclusion list** — clips from known password managers and other
//!   sensitive apps are dropped at parse time, before they ever reach the vault.
//!
//! ## Cursor
//!
//! `.trove/alfred-sync.json`: `cursor` = the highest `ts` value imported.
//! Persisted only after a successful write so a crash re-drains. If the DB is
//! absent (no Powerpack or Alfred not installed), the pass is a silent no-op.
//!
//! ## Text truncation
//!
//! Text clips are truncated at [`TEXT_MAX_BYTES`] characters to cap vault
//! growth — clipboard content that runs to pages (e.g. a pasted document) is
//! captured as a leading window. The `text_truncated` flag marks a cut.
//! Image and file clips store metadata only (app, type, hash); blob content is
//! never written to the vault.
//!
//! ## Dedupe
//!
//! `guid` = `dataHash` + `|` + `ts` (the cursor watermark is ts-based, so guids
//! don't need to be unique across the whole store — ts monotonicity ensures
//! the new window never re-imports the same row).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

/// Maximum number of UTF-8 characters we keep from a text clip. Clips longer
/// than this are stored with `text_truncated: true` and the prefix only.
const TEXT_MAX_CHARS: usize = 4000;

/// Hourly — clipboard items age out in as little as one day, so we poll
/// frequently. Matches the other always-on local collectors.
const ALFRED_SYNC_SECS: u64 = 3600;

const DIR: &str = "developer/alfred";
const SYNC_FILE: &str = ".trove/alfred-sync.json";

/// Apps whose clipboard clips we drop at parse time — they are likely to carry
/// password-manager data. Case-insensitive match on the `app` field.
const EXCLUDED_APPS: &[&str] = &[
    "1password",
    "keychain access",
    "bitwarden",
    "lastpass",
    "dashlane",
    "keepassxc",
    "keepass",
    "enpass",
    "nordpass",
    "roboform",
    "onepassword",
    "secrets",
    "strongbox",
];

// ---------------------------------------------------------------------------
// dataType constants.

const DTYPE_TEXT: i64 = 0;
const DTYPE_IMAGE: i64 = 2;
const DTYPE_FILE: i64 = 8;

// ---------------------------------------------------------------------------
// Registry face.

/// Path to Alfred's clipboard database (home-dir, no FDA needed).
fn alfred_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library/Application Support/Alfred/Databases/clipboard.alfdb")
    })
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_alfred()?;
    Ok(CollectOutcome::note_if(s.clips > 0, || {
        let mut note = format!("alfred clipboard synced — {} clips", s.clips);
        if s.excluded > 0 {
            note.push_str(&format!(" ({} excluded)", s.excluded));
        }
        note
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_alfred()?;
    let headline = if s.clips == 0 {
        "Alfred clipboard is up to date — no new clips".to_string()
    } else {
        format!(
            "Alfred clipboard synced — {} clips ({} excluded)",
            s.clips, s.excluded
        )
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("clips", s.clips), ("excluded", s.excluded)]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "alfred",
        name: "Alfred Clipboard",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Captures your Alfred Clipboard History — every item you've \
                      copied, with the app it came from and a timestamp. Requires \
                      Alfred Powerpack. Privacy-sensitive: opt in only if you want \
                      clipboard history preserved in your vault.",
        domain: "developer",
        vault_path: "developer/alfred/",
        toggleable: true,
        setup: &[
            "Requires Alfred Powerpack (~$34 one-time). Reads the clipboard database Alfred already keeps — nothing to install or connect.",
            "Enable only after confirming you're comfortable storing clipboard history in your vault. Passwords and secrets from password managers are filtered at collection time.",
            "Alfred's clipboard retention can be as short as one day — collect soon if you want historical data.",
        ],
        caveats: "Clipboard history is privacy-critical: it routinely carries passwords, \
                  tokens, and private text. An app-name exclusion list drops clips from \
                  password managers (1Password, Keychain Access, Bitwarden, etc.) before \
                  anything is written. Default retention is user-configurable from one day — \
                  uncollected history may age out before the next sync.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(ALFRED_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Row shape (raw, developer/ domain — no contract).

/// The dataType integer from Alfred's clipboard table, as a readable label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum ClipType {
    Text,
    Image,
    File,
    /// Any future/unknown dataType value.
    Unknown,
}

impl ClipType {
    fn from_dtype(d: i64) -> Self {
        match d {
            DTYPE_TEXT => ClipType::Text,
            DTYPE_IMAGE => ClipType::Image,
            DTYPE_FILE => ClipType::File,
            _ => ClipType::Unknown,
        }
    }
}

/// One row in `developer/alfred/YYYY-MM.jsonl`: a single clipboard event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ClipRow {
    /// Unique id: `<dataHash>|<ts>`.
    pub guid: String,
    /// RFC3339 timestamp (local time zone) of when the clip was recorded.
    pub ts: String,
    /// The clipboard content — text clips only. Omitted for image/file clips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// True when the text was truncated to [`TEXT_MAX_CHARS`] characters.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text_truncated: bool,
    /// The app name (e.g. "Safari", "Terminal").
    pub app: String,
    /// The app bundle path (e.g. "/Applications/Safari.app").
    pub app_bundle_path: String,
    /// The type of clipboard content.
    pub data_type: ClipType,
    /// Alfred's content hash — stable fingerprint of the raw clipboard data.
    pub data_hash: String,
}

/// Result of one collect pass.
#[derive(Debug, Clone, Default)]
pub struct AlfredStats {
    /// New rows written to the vault this pass.
    pub clips: u64,
    /// Rows dropped because the source app is on the exclusion list.
    pub excluded: u64,
}

// ---------------------------------------------------------------------------
// Cursor.

/// Incremental-sync state persisted in `.trove/alfred-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlfredSyncState {
    /// The `ts` value of the last row imported (Alfred's epoch-like decimal).
    #[serde(default)]
    pub cursor: f64,
    /// RFC3339 local time of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
}

// ---------------------------------------------------------------------------
// Parse helpers.

/// Is the `app` (case-insensitive) on the exclusion list?
fn is_excluded_app(app: &str) -> bool {
    let lower = app.to_ascii_lowercase();
    EXCLUDED_APPS.iter().any(|ex| lower.contains(ex))
}

/// Seconds between Unix epoch (1970-01-01) and Core Foundation / Mac epoch
/// (2001-01-01 UTC).  Alfred's `clipboard.alfdb` `ts` column stores
/// CFAbsoluteTime — seconds since 2001-01-01 — so we must add this offset
/// before calling `Local.timestamp_opt`.
///
/// Reference: the brief's cited gist
/// <https://gist.github.com/pirate/6551e1c00a7c4b0c607762930e22804c> (lines
/// 115-116) states "clipboard timestamps are in Mac epoch format … add
/// 978307200"; independently confirmed by rmoff.net/2020/05/18 where the
/// sample `ts=610489734` decodes to 2020-05-06 only with this offset.
const CF_EPOCH_OFFSET: i64 = 978_307_200;

/// Alfred stores timestamps as CFAbsoluteTime — seconds since 2001-01-01 UTC
/// (Mac / Core Foundation epoch), NOT Unix epoch.  Real Alfred `ts` values are
/// ~7.7e8 (e.g. 771_731_000 ≈ 2026-06-15).  Convert to RFC3339 local time by
/// first adding [`CF_EPOCH_OFFSET`] to shift to Unix epoch.
///
/// NOTE: the cursor persisted in `alfred-sync.json` remains in raw Alfred CF
/// units; the offset is applied only at display / partition-key time.
fn alfred_ts_to_rfc3339(ts: f64) -> Option<String> {
    let unix = ts + CF_EPOCH_OFFSET as f64;
    let secs = unix as i64;
    let nanos = ((unix - secs as f64) * 1_000_000_000.0) as u32;
    Local.timestamp_opt(secs, nanos).single().map(|dt| dt.to_rfc3339())
}

/// Truncate text to at most [`TEXT_MAX_CHARS`] Unicode characters. Returns
/// (truncated_text, was_truncated).
fn truncate_text(s: &str) -> (&str, bool) {
    let mut char_indices = s.char_indices();
    let end = char_indices.nth(TEXT_MAX_CHARS);
    match end {
        Some((idx, _)) => (&s[..idx], true),
        None => (s, false),
    }
}

/// Build a [`ClipRow`] from the raw DB values. Returns `None` when the row
/// should be dropped (excluded app, NULL hash, unrepresentable timestamp).
fn build_row(
    ts: f64,
    item: Option<String>,
    app: &str,
    apppath: &str,
    data_type: i64,
    data_hash: &str,
) -> Option<ClipRow> {
    // Drop excluded apps at parse time — they never reach the vault.
    if is_excluded_app(app) {
        return None;
    }
    // A row without a usable timestamp is dropped (degenerate DB row).
    let ts_str = alfred_ts_to_rfc3339(ts)?;
    // Guid: stable per-clip fingerprint.
    let guid = format!("{}|{}", data_hash, ts as i64);

    let (text, text_truncated) = if data_type == DTYPE_TEXT {
        if let Some(raw) = item {
            let (body, cut) = truncate_text(&raw);
            (Some(body.to_string()), cut)
        } else {
            (None, false)
        }
    } else {
        // Image / file / unknown: metadata only, no content.
        (None, false)
    };

    Some(ClipRow {
        guid,
        ts: ts_str,
        text,
        text_truncated,
        app: app.to_string(),
        app_bundle_path: apppath.to_string(),
        data_type: ClipType::from_dtype(data_type),
        data_hash: data_hash.to_string(),
    })
}

// ---------------------------------------------------------------------------
// SQLite import.

/// Import all rows from `db` with `ts > cursor`. Returns (rows written, rows
/// excluded, new max ts).
fn import_alfred_db(
    vault: &Vault,
    db: &std::path::Path,
    cursor: f64,
) -> Result<(u64, u64, f64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening alfred clipboard copy {}", db.display()))?;

    let mut stmt = conn.prepare(
        "SELECT ts, item, app, apppath, dataType, COALESCE(dataHash, '') \
         FROM clipboard \
         WHERE ts > ?1 \
         ORDER BY ts ASC",
    )?;
    let mut rows_iter = stmt.query([cursor])?;

    let mut clips: Vec<ClipRow> = Vec::new();
    let mut excluded = 0u64;
    let mut max_ts = cursor;

    while let Some(row) = rows_iter.next()? {
        let ts: f64 = row.get(0)?;
        let item: Option<String> = row.get(1)?;
        let app: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
        let apppath: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
        let data_type: i64 = row.get(4)?;
        let data_hash: String = row.get(5)?;

        max_ts = max_ts.max(ts);

        match build_row(ts, item, &app, &apppath, data_type, &data_hash) {
            Some(clip) => clips.push(clip),
            None => excluded += 1,
        }
    }

    let written = vault.append_alfred_clips(&clips)?;
    Ok((written, excluded, max_ts))
}

// ---------------------------------------------------------------------------
// Vault impl.

impl Vault {
    /// Read the persisted sync state. Missing or unreadable = defaults (cursor 0).
    fn read_alfred_sync(&self) -> AlfredSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_alfred_sync(&self, state: &AlfredSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Append a batch of clip rows to their month partitions. Returns count written.
    pub fn append_alfred_clips(&self, clips: &[ClipRow]) -> Result<u64> {
        if clips.is_empty() {
            return Ok(0);
        }
        let stream = self.stream(DIR, Partition::Month);
        stream.append(clips, |r| &r.ts)?;
        Ok(clips.len() as u64)
    }

    /// One incremental sync pass. Silently a no-op when Alfred is not installed
    /// or has no clipboard database.
    pub fn collect_alfred(&self) -> Result<AlfredStats> {
        let Some(db) = alfred_db_path() else {
            return Ok(AlfredStats::default());
        };
        if !db.is_file() {
            return Ok(AlfredStats::default());
        }
        self.collect_alfred_from(&db)
    }

    /// The pass itself, DB-path-injected for tests.
    pub(crate) fn collect_alfred_from(&self, db: &std::path::Path) -> Result<AlfredStats> {
        let mut state = self.read_alfred_sync();
        let cursor = state.cursor;
        let stem = format!("trove-alfred-{}", std::process::id());
        let (clips, excluded, new_cursor) =
            import_via_copy(db, &stem, |tmp| import_alfred_db(self, tmp, cursor))?;
        if new_cursor > cursor || clips > 0 {
            state.cursor = new_cursor;
            state.updated = Local::now().to_rfc3339();
            self.write_alfred_sync(&state)?;
        }
        Ok(AlfredStats { clips, excluded })
    }

    /// All clip rows for one month partition (`YYYY-MM`). Used by tests and the
    /// Recent-data view.
    pub fn alfred_clips(&self, month: &str) -> Result<Vec<ClipRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Unique temp vault per test.
    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-alfred-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Synthetic SQLite helpers.

    /// Write a minimal synthetic clipboard.alfdb with the real schema + supplied
    /// rows. Confirmed against the actual Alfred schema (2026-06-16).
    fn write_synthetic_db(path: &std::path::Path, rows: &[(f64, Option<&str>, &str, &str, i64, &str)]) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE clipboard(item, ts decimal, app, apppath, dataType INTEGER, dataHash);
             CREATE INDEX clipboard_ts ON clipboard (ts);",
        ).unwrap();
        for (ts, item, app, apppath, dtype, dhash) in rows {
            conn.execute(
                "INSERT INTO clipboard(item, ts, app, apppath, dataType, dataHash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![item, ts, app, apppath, dtype, dhash],
            ).unwrap();
        }
    }

    /// Unique temp path for a synthetic DB.
    fn temp_db_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("trove-alfred-db-{}-{name}.alfdb", std::process::id()))
    }

    // -----------------------------------------------------------------------
    // Unit: CF epoch conversion.

    /// The CF epoch offset must produce the correct calendar date.
    ///
    /// Reference sample from rmoff.net/2020/05/18: `ts=610489734` (a real
    /// Alfred clipboard DB row) must decode to 2020-05-06 (the post date).
    /// Without the +978_307_200 offset it would decode to ~1989-05-06 (wrong).
    #[test]
    fn cf_epoch_rmoff_sample() {
        let ts_cf: f64 = 610_489_734.0;
        let rfc = alfred_ts_to_rfc3339(ts_cf).expect("should parse");
        // Must be in 2020, not 1989.
        assert!(
            rfc.starts_with("2020-"),
            "CF ts 610489734 must render as 2020-xx-xx, got: {rfc}"
        );
        // Specifically 2020-05-06 (UTC date; local may shift by ±1 day).
        assert!(
            rfc.starts_with("2020-05-0"),
            "CF ts 610489734 must render as 2020-05-0x, got: {rfc}"
        );
    }

    /// A realistic current-era CF timestamp (2026) must render as 2026, not 1995.
    #[test]
    fn cf_epoch_current_era_renders_correctly() {
        // CF ts for 2026-06-16: Unix(2026-06-16) - 978_307_200 = 803_260_800.
        // A naive (no-offset) implementation renders this as ~1995-06-16.
        let ts_cf: f64 = 803_260_800.0;
        let rfc = alfred_ts_to_rfc3339(ts_cf).expect("should parse");
        assert!(
            rfc.starts_with("2026-"),
            "CF ts 803260800 must render as 2026-xx-xx, got: {rfc}"
        );
    }

    // -----------------------------------------------------------------------
    // Unit: build_row / exclusion / truncation.
    //
    // All fixtures below use CF-magnitude timestamps (~7.7e8) so the epoch
    // conversion is exercised on realistic values.

    /// CF ts that maps to a known 2026 date, used as the base for unit tests.
    /// CF 803_260_800 = Unix(2026-06-16 00:00:00 UTC) - 978_307_200.
    /// A naive (no-offset) implementation renders this as ~1995-06-16.
    const TEST_CF_TS: f64 = 803_260_800.0;

    #[test]
    fn build_row_text_clip() {
        let row = build_row(
            TEST_CF_TS,
            Some("hello world".to_string()),
            "Terminal",
            "/Applications/Utilities/Terminal.app",
            DTYPE_TEXT,
            "abc123",
        )
        .unwrap();
        assert_eq!(row.data_type, ClipType::Text);
        assert_eq!(row.text.as_deref(), Some("hello world"));
        assert!(!row.text_truncated);
        assert!(row.guid.starts_with("abc123|"));
        assert!(!row.app.is_empty());
        // Rendered ts must be in current era, not 1995.
        assert!(
            row.ts.starts_with("2026-"),
            "ts must render as 2026, got: {}",
            row.ts
        );
    }

    #[test]
    fn build_row_image_clip_no_text() {
        let row = build_row(
            TEST_CF_TS + 1.0,
            Some("BLOB-CONTENT-NEVER-STORED".to_string()),
            "Preview",
            "/Applications/Preview.app",
            DTYPE_IMAGE,
            "img_hash",
        )
        .unwrap();
        assert_eq!(row.data_type, ClipType::Image);
        // Image clips NEVER store content — even if item is non-None.
        assert_eq!(row.text, None, "image clip must not store content");
        assert!(!row.text_truncated);
    }

    #[test]
    fn build_row_file_clip_no_text() {
        let row = build_row(
            TEST_CF_TS + 2.0,
            Some("file:///Users/dave/doc.pdf".to_string()),
            "Finder",
            "/System/Library/CoreServices/Finder.app",
            DTYPE_FILE,
            "file_hash",
        )
        .unwrap();
        assert_eq!(row.data_type, ClipType::File);
        assert_eq!(row.text, None, "file clip must not store content");
    }

    #[test]
    fn build_row_excluded_app_returns_none() {
        // 1Password
        assert!(
            build_row(TEST_CF_TS, Some("pw".to_string()), "1Password 8", "/Applications/1Password.app", DTYPE_TEXT, "h1").is_none(),
            "1Password must be excluded"
        );
        // Keychain Access (case-insensitive)
        assert!(
            build_row(TEST_CF_TS, Some("key".to_string()), "Keychain Access", "/Applications/Utilities/Keychain Access.app", DTYPE_TEXT, "h2").is_none(),
            "Keychain Access must be excluded"
        );
        // Bitwarden
        assert!(
            build_row(TEST_CF_TS, Some("bw".to_string()), "Bitwarden", "/Applications/Bitwarden.app", DTYPE_TEXT, "h3").is_none(),
            "Bitwarden must be excluded"
        );
    }

    #[test]
    fn build_row_allowed_app_passes() {
        let row = build_row(
            TEST_CF_TS,
            Some("SELECT * FROM users".to_string()),
            "Sequel Pro",
            "/Applications/Sequel Pro.app",
            DTYPE_TEXT,
            "sql_hash",
        );
        assert!(row.is_some(), "non-excluded app must pass through");
    }

    #[test]
    fn text_truncation_at_limit() {
        // Build a string of exactly TEXT_MAX_CHARS + 10 chars.
        let long: String = "a".repeat(TEXT_MAX_CHARS + 10);
        let (out, truncated) = truncate_text(&long);
        assert_eq!(out.chars().count(), TEXT_MAX_CHARS);
        assert!(truncated);
        // A string at the limit is not truncated.
        let exact: String = "b".repeat(TEXT_MAX_CHARS);
        let (out2, truncated2) = truncate_text(&exact);
        assert_eq!(out2.len(), TEXT_MAX_CHARS);
        assert!(!truncated2);
    }

    #[test]
    fn build_row_long_text_sets_truncated_flag() {
        let long = "z".repeat(TEXT_MAX_CHARS + 1);
        let row = build_row(
            TEST_CF_TS,
            Some(long),
            "TextEdit",
            "/Applications/TextEdit.app",
            DTYPE_TEXT,
            "hash_long",
        )
        .unwrap();
        assert!(row.text_truncated, "text_truncated must be true for overlong clip");
        assert_eq!(
            row.text.as_ref().unwrap().chars().count(),
            TEXT_MAX_CHARS,
            "stored text must be exactly TEXT_MAX_CHARS chars"
        );
    }

    #[test]
    fn unknown_dtype_is_labelled_unknown() {
        let row = build_row(
            TEST_CF_TS,
            None,
            "SomeApp",
            "/Applications/SomeApp.app",
            99,
            "u_hash",
        )
        .unwrap();
        assert_eq!(row.data_type, ClipType::Unknown);
    }

    // -----------------------------------------------------------------------
    // Integration: synthetic DB → vault.

    #[test]
    fn collect_writes_clips_to_vault() {
        let v = temp_vault("collect");
        let db_path = temp_db_path("collect");

        // Realistic CF-epoch ts values: CF 803_260_800 = Unix(2026-06-16) - 978_307_200.
        // Using CF-magnitude (~8.0e8) instead of Unix-magnitude (~1.75e9) to exercise the
        // epoch conversion on realistic inputs (the previous bug used Unix-magnitude fixtures
        // which masked the off-by-31-years error — real Alfred ts is CF, ~8e8, not Unix ~1.75e9).
        let ts1: f64 = 803_260_800.0; // CF = 2026-06-16 00:00:00 UTC
        let ts2: f64 = 803_260_900.0;
        let ts3: f64 = 803_261_000.0; // image clip
        let ts4: f64 = 803_261_100.0; // excluded app

        write_synthetic_db(
            &db_path,
            &[
                (ts1, Some("hello vault"), "Terminal", "/Applications/Utilities/Terminal.app", DTYPE_TEXT, "hash_text_1"),
                (ts2, Some("SELECT 1"), "Sequel Pro", "/Applications/Sequel Pro.app", DTYPE_TEXT, "hash_text_2"),
                (ts3, None, "Preview", "/Applications/Preview.app", DTYPE_IMAGE, "hash_img_1"),
                (ts4, Some("pw123"), "1Password 8", "/Applications/1Password.app", DTYPE_TEXT, "hash_excluded"),
            ],
        );

        let stats = v.collect_alfred_from(&db_path).unwrap();
        assert_eq!(stats.clips, 3, "3 non-excluded clips written");
        assert_eq!(stats.excluded, 1, "1 excluded (1Password)");

        // Verify vault rows.
        let month = alfred_ts_to_rfc3339(ts1)
            .and_then(|t| Partition::Month.key(&t).map(|k| k.to_string()))
            .unwrap();
        let rows = v.alfred_clips(&month).unwrap();
        assert_eq!(rows.len(), 3);

        // Text clip 1: content preserved.
        let r1 = rows.iter().find(|r| r.data_hash == "hash_text_1").unwrap();
        assert_eq!(r1.text.as_deref(), Some("hello vault"));
        assert!(!r1.text_truncated);
        assert_eq!(r1.data_type, ClipType::Text);
        assert_eq!(r1.app, "Terminal");
        // CF epoch fix: ts must render as 2026, not 1995.
        assert!(
            r1.ts.starts_with("2026-"),
            "CF-epoch ts must render as 2026-xx-xx, got: {}",
            r1.ts
        );

        // Image clip: no text content stored.
        let r3 = rows.iter().find(|r| r.data_hash == "hash_img_1").unwrap();
        assert_eq!(r3.text, None, "image clip must not store content");
        assert_eq!(r3.data_type, ClipType::Image);

        // No excluded row in vault.
        assert!(
            rows.iter().all(|r| r.data_hash != "hash_excluded"),
            "excluded clip must not appear in vault"
        );

        // Cursor advanced.
        let state = v.read_alfred_sync();
        assert!(state.cursor >= ts4, "cursor advanced to max ts");

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn incremental_cursor_skips_already_imported() {
        let v = temp_vault("cursor");
        let db_path = temp_db_path("cursor");

        // CF-magnitude timestamps: ~8.0e8 (realistic Alfred DB values for 2026).
        let ts1: f64 = 803_262_000.0;
        let ts2: f64 = 803_263_000.0;

        write_synthetic_db(
            &db_path,
            &[
                (ts1, Some("first clip"), "Terminal", "/t", DTYPE_TEXT, "h_c1"),
                (ts2, Some("second clip"), "Terminal", "/t", DTYPE_TEXT, "h_c2"),
            ],
        );

        // First pass: imports both.
        let s1 = v.collect_alfred_from(&db_path).unwrap();
        assert_eq!(s1.clips, 2);

        // Second pass: cursor = ts2, nothing new.
        let s2 = v.collect_alfred_from(&db_path).unwrap();
        assert_eq!(s2.clips, 0, "second pass must import nothing (cursor guards)");

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn missing_db_is_silent_noop() {
        let v = temp_vault("missing");
        // Point at a path that does not exist.
        let absent =
            std::env::temp_dir().join(format!("trove-alfred-absent-{}.alfdb", std::process::id()));
        let stats = v.collect_alfred_from(&absent).unwrap_or_default();
        assert_eq!(stats.clips, 0);
        assert_eq!(stats.excluded, 0);
        // No vault directory created.
        assert!(!v.root().join(DIR).exists());
    }

    #[test]
    fn cursor_state_round_trips() {
        let empty: AlfredSyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.cursor, 0.0);
        assert!(empty.updated.is_empty());

        // cursor is stored in raw CF units (~8.0e8 for 2026).
        let s = AlfredSyncState {
            cursor: 803_260_800.5,
            updated: "2026-06-16T12:00:00+00:00".to_string(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AlfredSyncState = serde_json::from_str(&json).unwrap();
        assert!((back.cursor - s.cursor).abs() < 1.0);
        assert_eq!(back.updated, s.updated);
    }

    #[test]
    fn guid_includes_hash_and_ts() {
        // Use a CF-magnitude ts (the guid stores raw CF integer, not Unix).
        // CF 803_260_800 = Unix(2026-06-16 00:00:00 UTC) - 978_307_200.
        let cf_ts: f64 = 803_260_800.0;
        let row = build_row(
            cf_ts,
            Some("x".to_string()),
            "Safari",
            "/Applications/Safari.app",
            DTYPE_TEXT,
            "myhash",
        )
        .unwrap();
        assert!(
            row.guid.starts_with("myhash|"),
            "guid must start with dataHash: {}",
            row.guid
        );
        // guid uses the raw CF ts integer (not Unix, not the converted value).
        assert!(
            row.guid.contains("803260800"),
            "guid must contain raw CF ts: {}",
            row.guid
        );
    }

    #[test]
    fn clip_row_serialization_image_omits_text_fields() {
        let row = ClipRow {
            guid: "h|123".to_string(),
            ts: "2026-06-15T10:00:00+00:00".to_string(),
            text: None,
            text_truncated: false,
            app: "Preview".to_string(),
            app_bundle_path: "/Applications/Preview.app".to_string(),
            data_type: ClipType::Image,
            data_hash: "h".to_string(),
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(!json.contains("\"text\""), "text field must be omitted when None: {json}");
        assert!(!json.contains("text_truncated"), "text_truncated must be omitted when false: {json}");
    }
}
