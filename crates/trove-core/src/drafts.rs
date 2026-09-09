//! Drafts (Agiletortoise) — periodic read of Drafts' group-container SQLite
//! database (`DraftStore.sqlite`), modeled on [`crate::bear`].
//!
//! Needs **Full Disk Access** (same per-binary TCC grant as Bear / Voice Memos
//! / Messages); silently skipped while the database is unreadable.
//!
//! Writes the [`notes`](crate::notes) domain: a normalized **contract** layer
//! `notes/drafts/YYYY-MM.jsonl` (one [`Note`] per draft, partitioned by the
//! local month of `created`, deduped/upserted by `id` = the draft UUID) plus a
//! full-fidelity **raw** layer `notes/drafts/raw/YYYY-MM.jsonl` (every selected
//! `ZMANAGEDDRAFT` column verbatim, also partitioned by `created` month so the
//! raw row and contract row share the same file and cross-month edits upsert
//! cleanly — the same immutable-partition discipline as Bear).
//!
//! **Real schema (confirmed via `sqlite3 .schema` on a live DB):**
//!
//! ```sql
//! ZMANAGEDDRAFT (
//!   Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, Z_OPT INTEGER,
//!   ZFLAGGED INTEGER, ZFOLDER INTEGER, ZHIDDEN INTEGER,
//!   ZCREATED_AT TIMESTAMP, ZMODIFIED_AT TIMESTAMP, ZACCESSED_AT TIMESTAMP,
//!   ZCREATED_LATITUDE FLOAT, ZCREATED_LONGITUDE FLOAT,
//!   ZMODIFIED_LATITUDE FLOAT, ZMODIFIED_LONGITUDE FLOAT,
//!   ZCACHED_TAGS VARCHAR,    -- "ZZZ<tag1>ZZZtag2>ZZZ" sentinel-delimited
//!   ZCHANGE_TAG VARCHAR, ZCONTENT VARCHAR, ZCREATED_DEVICE VARCHAR,
//!   ZLANGUAGE_GRAMMAR_NAME VARCHAR, ZMODIFIED_DEVICE VARCHAR,
//!   ZTITLE VARCHAR,          -- always empty; Drafts derives title from first line
//!   ZUUID VARCHAR,           -- stable per-draft UUID — the dedupe key
//!   ZMETADATA BLOB,
//!   ZFLAG_TYPE INTEGER, ZLOCK_TYPE INTEGER, ZLOCK_KEY VARCHAR,
//!   ZTITLE_OVERRIDE INTEGER, ZSNOOZE_UNTIL TIMESTAMP
//! )
//!
//! ZMANAGEDDRAFTTAG (
//!   Z_PK INTEGER PRIMARY KEY, ZHIDDEN INTEGER,
//!   ZDRAFT_UUID VARCHAR,     -- FK → ZMANAGEDDRAFT.ZUUID
//!   ZNAME VARCHAR            -- the tag name
//! )
//! ```
//!
//! **ZFOLDER codes:** 0 = inbox, 1 = archive, 10000 = trash.
//! **ZHIDDEN:** 0 = visible in-app, 1 = soft-deleted sync tombstone — we skip
//! ZHIDDEN=1 rows entirely (they are internal sync metadata, not user content).
//! **Timestamps:** Core Data seconds since 2001-01-01 (UTC), same as Bear —
//! add 978 307 200 for Unix epoch. Drafts' ZCREATED_AT uses the same Apple
//! epoch convention confirmed on real data (803 250 448 → 2026-06-15 UTC).
//! **Tags:** `ZCACHED_TAGS` is a `ZZZ`-delimited list (`ZZZtagAZZZtagBZZZ`);
//! we fall back to the `ZMANAGEDDRAFTTAG` join table to handle any DB state
//! where the cache is stale, empty, or absent.
//!
//! Incremental sync watermarks on `ZMODIFIED_AT` in `.trove/drafts-sync.json`.
//! First run = full backfill. Dedupe is by UUID (whole affected-month rewrite),
//! so a lost watermark never duplicates a draft.
//!
//! **Title derivation:** `ZTITLE` is always empty on this schema version —
//! Drafts synthesizes the display title from the first non-empty line at read
//! time. We replicate that: first line of `ZCONTENT` that is non-empty and
//! ≤ 120 chars, with a `# ` / `## ` Markdown heading prefix stripped.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Seconds between Drafts syncs — hourly, same cadence as Bear.
pub const DRAFTS_SYNC_SECS: u64 = 3600;

/// Apple / Core Data epoch offset: seconds from 1970-01-01 to 2001-01-01 UTC.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

/// ZFOLDER constant: inbox.
const FOLDER_INBOX: i64 = 0;
/// ZFOLDER constant: archive.
const FOLDER_ARCHIVE: i64 = 1;
/// ZFOLDER constant: trash.
const FOLDER_TRASH: i64 = 10_000;

const SYNC_FILE: &str = ".trove/drafts-sync.json";
const SOURCE: &str = "drafts";
const NOTES_DIR: &str = "notes/drafts";
const RAW_DIR: &str = "notes/drafts/raw";

// ---------------------------------------------------------------------------
// Registry callbacks (must be `fn` pointers, not closures).

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_drafts()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_notes > 0, || {
        format!("imported {} Drafts notes", s.new_notes)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(drafts_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_drafts_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "drafts",
        name: "Drafts",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads your Drafts texts and tags from Drafts' local database \
                      every hour; the first sync backfills your whole library. \
                      Only visible (non-hidden) drafts are collected; trashed and \
                      archived drafts are preserved with their state.",
        domain: "notes",
        vault_path: "notes/drafts/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats: "Requires Full Disk Access. Internal sync tombstones (ZHIDDEN=1 rows) \
                  are skipped. Trashed and archived drafts are preserved with their \
                  folder state.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(DRAFTS_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Persisted sync state in `.trove/drafts-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DraftsSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest `ZMODIFIED_AT` (Core Data seconds) imported so far.
    pub cursor: f64,
}

/// Result of one sync pass.
#[derive(Debug, Clone, Serialize)]
pub struct DraftsSyncStats {
    /// False when the database is unreadable (no FDA or Drafts never used).
    pub available: bool,
    pub new_notes: u64,
}

// ---------------------------------------------------------------------------
// Home / path resolution

fn home_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// Drafts group-container DB path.
fn drafts_db_path() -> Option<PathBuf> {
    home_root().map(|h| {
        h.join("Library/Group Containers/GTFQ98J4YG.com.agiletortoise.Drafts/DraftStore.sqlite")
    })
}

/// Whether this process can read the Drafts database.
pub fn drafts_permission_ok() -> bool {
    drafts_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Core Data date

/// Core Data seconds since 2001 (may be fractional) → local [`DateTime`].
/// Returns `None` for non-finite values.
fn core_data_to_local(z: f64) -> Option<DateTime<Local>> {
    if !z.is_finite() {
        return None;
    }
    let secs = z.trunc() as i64 + APPLE_EPOCH_OFFSET_S;
    let nanos = (z.fract().abs() * 1_000_000_000.0).round() as u32;
    DateTime::from_timestamp(secs, nanos).map(|t| t.with_timezone(&Local))
}

// ---------------------------------------------------------------------------
// Tag loading — join-table is primary; ZCACHED_TAGS as a belt-and-suspenders
// sanity check only (we never trust cached data over a real table row).

/// Load all non-hidden tags per draft UUID from `ZMANAGEDDRAFTTAG`.
fn load_tags(conn: &rusqlite::Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    // ZHIDDEN=1 rows in the tag table are sync tombstones (same convention as
    // the draft table) — skip them.
    let mut stmt = conn.prepare(
        "SELECT ZDRAFT_UUID, ZNAME FROM ZMANAGEDDRAFTTAG \
         WHERE ZHIDDEN=0 AND ZDRAFT_UUID IS NOT NULL AND ZNAME IS NOT NULL \
         ORDER BY ZDRAFT_UUID, ZNAME",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let uuid: String = row.get("ZDRAFT_UUID")?;
        let name: String = row.get("ZNAME")?;
        if !name.is_empty() {
            map.entry(uuid).or_default().push(name);
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Title derivation

/// Extract the display title from a Drafts body: the first non-empty line
/// that is ≤ 120 chars, with a leading Markdown heading prefix (`# ` / `## ` /
/// etc.) stripped. Returns an empty string when the body has no usable line.
fn derive_title(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Strip markdown heading prefix (one or more `#` followed by a space).
        let stripped = if let Some(rest) = trimmed.strip_prefix('#') {
            rest.trim_start_matches('#').trim_start_matches(' ')
        } else {
            trimmed
        };
        let candidate = stripped.trim();
        if !candidate.is_empty() && candidate.len() <= 120 {
            return candidate.to_string();
        }
        // First non-empty line that is too long: take the first 120 chars as a
        // title stub rather than silently omitting it.
        if !candidate.is_empty() {
            return candidate.chars().take(120).collect();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Folder → vault label

fn folder_label(folder_code: Option<i64>) -> Option<&'static str> {
    match folder_code {
        Some(FOLDER_INBOX) => Some("inbox"),
        Some(FOLDER_ARCHIVE) => Some("archive"),
        Some(FOLDER_TRASH) => Some("trash"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Core import

/// Read drafts from a DB (or a copy) and upsert into both contract and raw
/// layers. Returns `(count, new_max_cursor)`.
fn import_drafts_db(vault: &Vault, db: &Path, cursor: f64) -> Result<(u64, f64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening Drafts DB copy {}", db.display()))?;
    let tags_by_uuid = load_tags(&conn)?;

    // We query all non-hidden drafts modified after the cursor. ZHIDDEN=0
    // filters out sync tombstones; the watermark is on ZMODIFIED_AT.
    let sql = "SELECT \
        Z_PK, ZUUID, ZCONTENT, ZCREATED_AT, ZMODIFIED_AT, \
        ZFLAGGED, ZFOLDER, ZHIDDEN, ZCACHED_TAGS, \
        ZCREATED_LATITUDE, ZCREATED_LONGITUDE, \
        ZMODIFIED_LATITUDE, ZMODIFIED_LONGITUDE, \
        ZLANGUAGE_GRAMMAR_NAME, ZCREATED_DEVICE \
        FROM ZMANAGEDDRAFT \
        WHERE ZHIDDEN=0 AND ZMODIFIED_AT > ?1 \
        ORDER BY ZMODIFIED_AT";

    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([cursor])?;

    let mut contract: Vec<Note> = Vec::new();
    let mut raw: Vec<Value> = Vec::new();
    let mut max = cursor;

    while let Some(row) = rows.next()? {
        // UUID is the stable dedupe key — skip rows without one.
        let uuid: Option<String> = row.get("ZUUID")?;
        let Some(uuid) = uuid.filter(|s| !s.is_empty()) else {
            continue;
        };

        let z_mod: Option<f64> = row
            .get::<_, Option<f64>>("ZMODIFIED_AT")?
            .filter(|z| z.is_finite());
        if let Some(z) = z_mod {
            max = max.max(z);
        }

        let z_created: Option<f64> = row
            .get::<_, Option<f64>>("ZCREATED_AT")?
            .filter(|z| z.is_finite());

        let modified = z_mod.and_then(core_data_to_local).map(|t| t.to_rfc3339());
        let created = z_created
            .and_then(core_data_to_local)
            .map(|t| t.to_rfc3339())
            .or_else(|| modified.clone());

        // A draft with no usable timestamp can't be filed into a month partition
        // — skip it (extremely rare; every real draft has a ZCREATED_AT).
        let Some(created) = created else {
            continue;
        };

        let content: Option<String> = row.get("ZCONTENT")?;
        let content_str = content.as_deref().unwrap_or("").trim().to_string();

        let z_pk: i64 = row.get("Z_PK")?;
        let folder: Option<i64> = row.get("ZFOLDER")?;
        let flagged: Option<i64> = row.get("ZFLAGGED")?;

        // Classify as trashed / archived based on ZFOLDER code.
        let is_trashed = folder == Some(FOLDER_TRASH);
        let is_archived = folder == Some(FOLDER_ARCHIVE);

        // Tags from the join table (authoritative); ZCACHED_TAGS is supplemental.
        let tags: Vec<String> = if let Some(t) = tags_by_uuid.get(&uuid) {
            t.clone()
        } else {
            // Fallback: parse ZCACHED_TAGS ("ZZZ<tag>ZZZ" sentinel-delimited).
            let cached: Option<String> = row.get("ZCACHED_TAGS")?;
            parse_cached_tags(cached.as_deref())
        };

        // Contract row.
        let mut note = Note::new(SOURCE, &uuid);
        note.created = created.clone();
        if let Some(m) = &modified {
            note.modified = m.clone();
        }
        if !content_str.is_empty() {
            let title = derive_title(&content_str);
            if !title.is_empty() {
                note.title = title;
            }
            note.body = content_str.clone();
        }
        note.tags = tags.clone();
        if let Some(label) = folder_label(folder) {
            note.folder = label.to_string();
        }
        if flagged == Some(1) {
            note.pinned = Some(true);
        }
        if is_archived {
            note.archived = Some(true);
        }
        if is_trashed {
            note.trashed = Some(true);
        }

        // Extra: source-specific fields the normalized columns don't carry.
        let mut extra = Map::new();
        if let Some(lg) = row.get::<_, Option<String>>("ZLANGUAGE_GRAMMAR_NAME")? {
            if !lg.is_empty() {
                extra.insert("language_grammar".into(), Value::String(lg));
            }
        }
        if let Some(dev) = row.get::<_, Option<String>>("ZCREATED_DEVICE")? {
            if !dev.is_empty() {
                extra.insert("created_device".into(), Value::String(dev));
            }
        }
        // Creation coordinates, when non-zero (Drafts optionally records where
        // the draft was started — privacy-neutral, user's own data).
        let lat: Option<f64> = row.get("ZCREATED_LATITUDE")?;
        let lon: Option<f64> = row.get("ZCREATED_LONGITUDE")?;
        if let (Some(la), Some(lo)) = (lat, lon) {
            if la.is_finite() && lo.is_finite() && (la != 0.0 || lo != 0.0) {
                extra.insert("created_latitude".into(), Value::from(la));
                extra.insert("created_longitude".into(), Value::from(lo));
            }
        }
        note.extra = extra;

        // Raw row: full fidelity, partitioned by `created` (immutable key so
        // cross-month edits don't orphan rows — same discipline as Bear).
        let mut raw_obj = Map::new();
        raw_obj.insert("source".into(), Value::from(SOURCE));
        raw_obj.insert("id".into(), Value::from(uuid.clone()));
        raw_obj.insert("z_pk".into(), Value::from(z_pk));
        raw_obj.insert("_created".into(), Value::from(created.clone()));
        if !content_str.is_empty() {
            raw_obj.insert("content".into(), Value::from(content_str));
        }
        if let Some(m) = &modified {
            raw_obj.insert("modified_at".into(), Value::from(m.clone()));
        }
        raw_obj.insert("created_at".into(), Value::from(created.clone()));
        if let Some(f) = folder {
            raw_obj.insert("folder".into(), Value::from(f));
        }
        if let Some(fg) = flagged {
            raw_obj.insert("flagged".into(), Value::from(fg));
        }
        if !tags.is_empty() {
            raw_obj.insert(
                "tags".into(),
                Value::Array(tags.iter().cloned().map(Value::from).collect()),
            );
        }
        if let Some(la) = lat {
            if la.is_finite() {
                raw_obj.insert("created_latitude".into(), Value::from(la));
            }
        }
        if let Some(lo) = lon {
            if lo.is_finite() {
                raw_obj.insert("created_longitude".into(), Value::from(lo));
            }
        }
        if let Some(lg) = row.get::<_, Option<String>>("ZLANGUAGE_GRAMMAR_NAME")? {
            if !lg.is_empty() {
                raw_obj.insert("language_grammar".into(), Value::from(lg));
            }
        }

        raw.push(Value::Object(raw_obj));
        contract.push(note);
    }
    drop(rows);
    drop(stmt);

    let n = contract.len() as u64;
    if !contract.is_empty() {
        vault.upsert_drafts_notes(&contract)?;
        vault.upsert_drafts_raw(&raw)?;
    }
    Ok((n, max))
}

/// Parse the `ZCACHED_TAGS` field: `"ZZZtag1ZZZtag2ZZZ"` → `["tag1", "tag2"]`.
/// Returns an empty vec when the value is None, empty, or has no tags.
fn parse_cached_tags(cached: Option<&str>) -> Vec<String> {
    let Some(s) = cached else {
        return Vec::new();
    };
    // Split on "ZZZ": `"ZZZaZZZbZZZ".split("ZZZ")` → ["", "a", "b", ""]
    s.split("ZZZ")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One incremental sync pass over the Drafts database.
    pub fn collect_drafts(&self) -> Result<DraftsSyncStats> {
        if !drafts_permission_ok() {
            return Ok(DraftsSyncStats { available: false, new_notes: 0 });
        }
        let db = drafts_db_path().expect("permission_ok implies path");
        let mut state = self.read_drafts_sync().unwrap_or_default();
        let stem = format!("trove-drafts-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| import_drafts_db(self, tmp, state.cursor))?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_drafts_sync(&state)?;
        Ok(DraftsSyncStats { available: true, new_notes: n })
    }

    /// Upsert notes into `notes/drafts/YYYY-MM.jsonl`, partitioned by
    /// `created` month, deduped by `id`. Each affected month is rewritten whole.
    pub fn upsert_drafts_notes(&self, notes: &[Note]) -> Result<()> {
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            if let Some(key) = Partition::Month.key(&n.created) {
                by_month.entry(key.to_string()).or_default().push(n);
            }
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|n| n.id.as_str()).collect();
            let mut merged: Vec<Note> = stream
                .read::<Note>(&month)?
                .into_iter()
                .filter(|e| !incoming_ids.contains(e.id.as_str()))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{NOTES_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Upsert raw rows into `notes/drafts/raw/YYYY-MM.jsonl`, partitioned by
    /// `_created`, deduped by `id`. Each affected month is rewritten whole.
    pub fn upsert_drafts_raw(&self, rows: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in rows {
            if let Some(key) = Partition::Month.key(month_of(v)) {
                by_month.entry(key.to_string()).or_default().push(v);
            }
        }
        let stream = self.stream(RAW_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|v| id_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|e| !incoming_ids.contains(id_of(e)))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{RAW_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_drafts_sync(&self) -> Option<DraftsSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_drafts_sync(&self, state: &DraftsSyncState) -> Result<()> {
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

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-drafts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Core Data seconds for a fixed local datetime.
    fn z_date(y: i32, m: u32, d: u32, h: u32) -> f64 {
        let t = Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) as f64
    }

    /// Build a synthetic Drafts DB with the real `ZMANAGEDDRAFT` + tag tables.
    fn fake_drafts_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-drafts-db-{}-{name}.sqlite",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZMANAGEDDRAFT (
                Z_PK INTEGER PRIMARY KEY,
                Z_ENT INTEGER, Z_OPT INTEGER,
                ZFLAGGED INTEGER,
                ZFOLDER INTEGER,
                ZHIDDEN INTEGER,
                ZCREATED_AT REAL,
                ZMODIFIED_AT REAL,
                ZACCESSED_AT REAL,
                ZCREATED_LATITUDE REAL,
                ZCREATED_LONGITUDE REAL,
                ZMODIFIED_LATITUDE REAL,
                ZMODIFIED_LONGITUDE REAL,
                ZCACHED_TAGS TEXT,
                ZCHANGE_TAG TEXT,
                ZCONTENT TEXT,
                ZCREATED_DEVICE TEXT,
                ZLANGUAGE_GRAMMAR_NAME TEXT,
                ZMODIFIED_DEVICE TEXT,
                ZTITLE TEXT,
                ZUUID TEXT,
                ZMETADATA BLOB,
                ZFLAG_TYPE INTEGER,
                ZLOCK_TYPE INTEGER,
                ZLOCK_KEY TEXT,
                ZTITLE_OVERRIDE INTEGER,
                ZSNOOZE_UNTIL REAL
            );
            CREATE TABLE ZMANAGEDDRAFTTAG (
                Z_PK INTEGER PRIMARY KEY,
                Z_ENT INTEGER, Z_OPT INTEGER,
                ZHIDDEN INTEGER,
                ZCHANGE_TAG TEXT,
                ZDRAFT_UUID TEXT,
                ZNAME TEXT
            );",
        )
        .unwrap();
        drop(conn);
        path
    }

    fn conn(path: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(path).unwrap()
    }

    fn insert_draft(
        c: &rusqlite::Connection,
        pk: i64,
        uuid: &str,
        content: &str,
        created: f64,
        modified: f64,
        folder: i64,
        flagged: i64,
        hidden: i64,
        cached_tags: Option<&str>,
    ) {
        c.execute(
            "INSERT INTO ZMANAGEDDRAFT (Z_PK, ZUUID, ZCONTENT, ZCREATED_AT, ZMODIFIED_AT, ZFOLDER, ZFLAGGED, ZHIDDEN, ZCACHED_TAGS)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![pk, uuid, content, created, modified, folder, flagged, hidden, cached_tags],
        )
        .unwrap();
    }

    fn insert_tag(c: &rusqlite::Connection, pk: i64, draft_uuid: &str, name: &str) {
        c.execute(
            "INSERT INTO ZMANAGEDDRAFTTAG (Z_PK, ZDRAFT_UUID, ZNAME, ZHIDDEN) VALUES (?1, ?2, ?3, 0)",
            rusqlite::params![pk, draft_uuid, name],
        )
        .unwrap();
    }

    // -----------------------------------------------------------------------
    // Unit tests

    #[test]
    fn core_data_epoch_conversion() {
        // 2026-06-15T00:00:00Z = Unix 1_781_568_000
        // Core Data value = 1_781_568_000 - 978_307_200 = 803_260_800
        let z = 803_260_800.0_f64;
        assert_eq!(core_data_to_local(z).unwrap().timestamp(), 1_781_568_000);
        assert!(core_data_to_local(f64::NAN).is_none());
        assert!(core_data_to_local(f64::INFINITY).is_none());
    }

    #[test]
    fn parse_cached_tags_zzz_delimiter() {
        assert_eq!(parse_cached_tags(Some("ZZZpros/consZZZ")), vec!["pros/cons"]);
        assert_eq!(
            parse_cached_tags(Some("ZZZlistsZZZworkZZZ")),
            vec!["lists", "work"]
        );
        assert_eq!(parse_cached_tags(Some("")), Vec::<String>::new());
        assert_eq!(parse_cached_tags(None), Vec::<String>::new());
    }

    #[test]
    fn derive_title_first_line_stripping() {
        assert_eq!(derive_title("## Garden plan\n\nDetails"), "Garden plan");
        assert_eq!(derive_title("# Heading\n"), "Heading");
        assert_eq!(derive_title("\n\nFirst non-empty"), "First non-empty");
        assert_eq!(derive_title("Plain first line\nmore"), "Plain first line");
        assert_eq!(derive_title(""), "");
        // Very long first line gets truncated to 120 chars.
        let long = "A".repeat(200);
        assert_eq!(derive_title(&long).len(), 120);
    }

    #[test]
    fn imports_inbox_archived_trashed_and_flagged() {
        let v = temp_vault("basic");
        let db = fake_drafts_db("basic");
        let c = conn(&db);

        // Inbox, flagged, with tags (via join table).
        insert_draft(
            &c, 1, "UUID-A",
            "# Quick capture\n\nBuy milk",
            z_date(2026, 3, 14, 9), z_date(2026, 4, 2, 18),
            FOLDER_INBOX, 1, 0, None,
        );
        insert_tag(&c, 1, "UUID-A", "shopping");
        insert_tag(&c, 2, "UUID-A", "home");

        // Archive note.
        insert_draft(
            &c, 2, "UUID-B",
            "Old reference",
            z_date(2026, 5, 1, 8), z_date(2026, 5, 1, 8),
            FOLDER_ARCHIVE, 0, 0, None,
        );

        // Trash note.
        insert_draft(
            &c, 3, "UUID-C",
            "Discard me",
            z_date(2026, 5, 2, 8), z_date(2026, 5, 2, 8),
            FOLDER_TRASH, 0, 0, None,
        );

        // Hidden sync tombstone — must NOT be imported.
        insert_draft(
            &c, 4, "UUID-HIDDEN",
            "tombstone",
            z_date(2026, 5, 3, 8), z_date(2026, 5, 3, 8),
            FOLDER_INBOX, 0, 1, None,
        );
        drop(c);

        let (n, _) = import_drafts_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 3, "hidden tombstone excluded");

        // March file: UUID-A (created March, modified April — partitioned by created).
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar.len(), 1);
        let a = &mar[0];
        assert_eq!(a.source, "drafts");
        assert_eq!(a.id, "UUID-A");
        assert_eq!(a.title, "Quick capture");
        assert!(a.body.contains("Buy milk"));
        assert_eq!(a.tags, vec!["home", "shopping"], "tags alphabetically from join table");
        assert_eq!(a.pinned, Some(true));
        assert_eq!(a.folder, "inbox");
        assert!(a.created.starts_with("2026-03-14"));
        assert!(a.modified.starts_with("2026-04-02"), "modified preserved across months");
        assert_eq!(a.archived, None);
        assert_eq!(a.trashed, None);

        // May file: UUID-B (archive) and UUID-C (trash).
        let may = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may.len(), 2);
        let b = may.iter().find(|n| n.id == "UUID-B").unwrap();
        assert_eq!(b.archived, Some(true));
        assert_eq!(b.folder, "archive");
        let c_note = may.iter().find(|n| n.id == "UUID-C").unwrap();
        assert_eq!(c_note.trashed, Some(true));
        assert_eq!(c_note.folder, "trash");

        // Raw layer — partitioned by created (immutable), not modified.
        // UUID-A created March, modified April → raw row is in March, NOT April.
        let raw_mar = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-03").unwrap();
        assert_eq!(raw_mar.len(), 1);
        assert_eq!(raw_mar[0]["id"], "UUID-A");
        assert_eq!(raw_mar[0]["folder"], FOLDER_INBOX as i64);
        assert!(raw_mar[0]["content"].as_str().unwrap().contains("Buy milk"));
        assert!(raw_mar[0]["z_pk"].as_i64().is_some(), "raw keeps z_pk for fidelity");
        // No raw row leaked into April (modification month).
        assert!(
            v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-04").unwrap().is_empty(),
            "raw partitioned by created, not modified"
        );

        let _ = fs::remove_file(db);
    }

    #[test]
    fn incremental_watermark_and_upsert() {
        let v = temp_vault("incremental");
        let db = fake_drafts_db("incremental");
        let c = conn(&db);
        insert_draft(
            &c, 1, "UUID-INC", "First body",
            z_date(2026, 6, 1, 9), z_date(2026, 6, 1, 9),
            FOLDER_INBOX, 0, 0, None,
        );
        drop(c);

        let (n1, max1) = import_drafts_db(&v, &db, 0.0).unwrap();
        assert_eq!(n1, 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june[0].body, "First body");

        // Re-run from watermark: nothing new.
        let (n2, max2) = import_drafts_db(&v, &db, max1).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(max2, max1);

        // Edit the draft: same UUID, later modified.
        let c = conn(&db);
        c.execute(
            "UPDATE ZMANAGEDDRAFT SET ZCONTENT='Edited body', ZMODIFIED_AT=?1 WHERE Z_PK=1",
            [z_date(2026, 6, 5, 12)],
        )
        .unwrap();
        drop(c);

        let (n3, _) = import_drafts_db(&v, &db, max1).unwrap();
        assert_eq!(n3, 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june.len(), 1, "upsert by UUID: exactly one line, not two");
        assert_eq!(june[0].body, "Edited body");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn cross_month_edit_leaves_exactly_one_row_in_created_month() {
        let v = temp_vault("crossmonth");
        let db = fake_drafts_db("crossmonth");
        let c = conn(&db);
        insert_draft(
            &c, 1, "UUID-X", "march body",
            z_date(2026, 3, 10, 9), z_date(2026, 3, 10, 9),
            FOLDER_INBOX, 0, 0, None,
        );
        drop(c);
        let (_, max1) = import_drafts_db(&v, &db, 0.0).unwrap();

        let c = conn(&db);
        c.execute(
            "UPDATE ZMANAGEDDRAFT SET ZCONTENT='june body', ZMODIFIED_AT=?1 WHERE Z_PK=1",
            [z_date(2026, 6, 20, 14)],
        )
        .unwrap();
        drop(c);
        let (n, _) = import_drafts_db(&v, &db, max1).unwrap();
        assert_eq!(n, 1);

        // Contract: one row in March (created month), no orphan in June.
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar.len(), 1);
        assert_eq!(mar[0].body, "june body");
        assert!(mar[0].modified.starts_with("2026-06-20"));
        assert!(
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap().is_empty(),
            "no orphan in June contract"
        );

        // Raw: one row in March, no orphan in June.
        let raw_mar = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-03").unwrap();
        assert_eq!(raw_mar.len(), 1);
        assert_eq!(raw_mar[0]["content"], "june body");
        assert!(
            v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-06").unwrap().is_empty(),
            "no orphan in June raw"
        );

        let _ = fs::remove_file(db);
    }

    #[test]
    fn null_modified_at_excluded_by_watermark_filter() {
        // A draft whose ZMODIFIED_AT is NULL is excluded by `ZMODIFIED_AT > cursor`
        // (NULL comparisons yield NULL in SQL, not true) — never fabricates a
        // 2001-01-01 row or advances the watermark to 0.
        let v = temp_vault("nullmod");
        let db = fake_drafts_db("nullmod");
        let c = conn(&db);
        c.execute(
            "INSERT INTO ZMANAGEDDRAFT (Z_PK, ZUUID, ZCONTENT, ZCREATED_AT, ZMODIFIED_AT, ZFOLDER, ZHIDDEN)
             VALUES (1, 'UUID-NULL', 'null mod body', ?1, NULL, 0, 0)",
            [z_date(2026, 4, 1, 9)],
        )
        .unwrap();
        insert_draft(
            &c, 2, "UUID-OK", "has mod",
            z_date(2026, 6, 2, 9), z_date(2026, 6, 2, 9),
            FOLDER_INBOX, 0, 0, None,
        );
        drop(c);

        let (n, max) = import_drafts_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1, "NULL-mod row excluded");
        assert!(
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2001-01").unwrap().is_empty(),
            "no 2001 row from NULL→0.0 coercion"
        );
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june[0].id, "UUID-OK");
        assert!(max > 0.0, "watermark is a real date, not 0.0");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn cached_tags_fallback_when_no_join_rows() {
        // When ZMANAGEDDRAFTTAG has no rows for a draft, fall back to ZCACHED_TAGS.
        let v = temp_vault("cachedtags");
        let db = fake_drafts_db("cachedtags");
        let c = conn(&db);
        insert_draft(
            &c, 1, "UUID-CACHED", "content",
            z_date(2026, 6, 1, 9), z_date(2026, 6, 1, 9),
            FOLDER_INBOX, 0, 0, Some("ZZZlistsZZZworkZZZ"),
        );
        // No rows in ZMANAGEDDRAFTTAG for this UUID.
        drop(c);

        let (n, _) = import_drafts_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june[0].tags, vec!["lists", "work"], "fallback to ZCACHED_TAGS");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn watermark_persists_and_round_trips() {
        let v = temp_vault("watermark");
        assert!(v.read_drafts_sync().is_none());
        let state = DraftsSyncState {
            updated: "2026-06-14T00:00:00-07:00".into(),
            cursor: 803_260_800.0,
        };
        v.write_drafts_sync(&state).unwrap();
        let got = v.read_drafts_sync().unwrap();
        assert_eq!(got.cursor, 803_260_800.0);
        assert_eq!(got.updated, "2026-06-14T00:00:00-07:00");
    }

    #[test]
    fn fda_unreadable_is_graceful_no_op() {
        let _g = env_guard();
        let fake_home =
            std::env::temp_dir().join(format!("trove-drafts-nohome-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fake_home);
        fs::create_dir_all(&fake_home).unwrap();
        std::env::set_var("TROVE_HOME", &fake_home);

        assert!(!drafts_permission_ok(), "no DB under empty home");
        let v = temp_vault("noop");
        let stats = v.collect_drafts().unwrap();
        assert!(!stats.available);
        assert_eq!(stats.new_notes, 0);

        std::env::remove_var("TROVE_HOME");
    }

    #[test]
    fn serde_back_compat_old_lines_still_deserialize() {
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-06.jsonl")),
            "{\"source\":\"drafts\",\"id\":\"OLD-1\"}\n{\"source\":\"drafts\",\"id\":\"OLD-2\",\"body\":\"b\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "OLD-1");
        assert_eq!(rows[1].body, "b");
    }

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
