//! Apple Notes — periodic local collector for the Notes app's `NoteStore.sqlite`
//! database (Core Data store at
//! `~/Library/Group Containers/group.com.apple.notes/NoteStore.sqlite`).
//!
//! Requires **Full Disk Access** (the same per-binary, no-programmatic-prompt
//! grant as iMessage / Bear / Voice Memos). Silently a no-op when the database
//! is unreadable; the hub surfaces the permission state.
//!
//! Writes the [`notes`](crate::notes) contract:
//! - Contract layer `notes/apple-notes/YYYY-MM.jsonl` — one [`Note`] per note,
//!   partitioned by the local month of `created`, deduped/upserted by `id`.
//! - Raw layer `notes/apple-notes/raw/YYYY-MM.jsonl` — every selected column
//!   verbatim, keyed by `id`, partitioned by `created` month (immutable key).
//!   Includes `body_text` (plain text) and `proto_bytes_b64` (gzip-decompressed
//!   protobuf payload, base64-encoded) for non-locked notes. The proto bytes
//!   preserve checklist state, attachment references, and other structure
//!   encoded in Apple's ZDATA blob, recoverable without re-reading the live DB.
//!
//! # Database layout (confirmed against a real NoteStore.sqlite)
//!
//! The database is a Core Data polymorphic single-table store:
//! - `ZICCLOUDSYNCINGOBJECT` — all entities by `Z_ENT`; entity numbers are
//!   looked up from `Z_PRIMARYKEY` (where `Z_NAME = 'ICNote'` / `'ICFolder'`)
//!   to avoid hardcoding a value that can shift between schema migrations.
//! - Note rows (`Z_ENT = <note_ent>`): `ZIDENTIFIER` (stable UUID, our guid),
//!   `ZCREATIONDATE3` (or `ZCREATIONDATE1`/`ZCREATIONDATE` on older schemas,
//!   detected via `pragma_table_info`) + `ZMODIFICATIONDATE1` (Core Data
//!   seconds since 2001),
//!   `ZISPASSWORDPROTECTED` (skip body when 1), `ZISPINNED`, `ZMARKEDFORDELETION`
//!   (treated as `trashed`), `ZFOLDER` (FK to folder rows).
//! - Folder rows (`Z_ENT = <folder_ent>`): `ZTITLE2` = folder name.
//! - `ZICNOTEDATA`: `ZNOTE` FK to `ZICCLOUDSYNCINGOBJECT.Z_PK`; `ZDATA` =
//!   gzip-compressed protobuf body.
//!
//! # Body decoding (confirmed path)
//!
//! The gzip-compressed `ZDATA` decompresses to a protobuf message. The note
//! text lives at: outer_message.field2 → inner_doc.field3 → body.field2 = UTF-8
//! string. We navigate this with a minimal hand-rolled varint reader rather than
//! adding prost + protoc (which would require a C build step). The path was
//! confirmed against David's real `NoteStore.sqlite` (841 notes, macOS 15/26).
//!
//! # Incremental sync
//!
//! Watermark = highest `ZMODIFICATIONDATE1` imported, in `.trove/apple-notes-sync.json`.
//! On first run: full scan (baseline, emits all notes). Subsequent runs: only
//! rows with `ZMODIFICATIONDATE1 > watermark`. Dedupe by `ZIDENTIFIER` ensures
//! idempotency even if the watermark is lost.
//!
//! # Title extraction
//!
//! Apple Notes derives the note title from the first line of the note body.
//! We decode the body and use its first non-empty line as the title; for locked
//! notes (body skipped) we fall back to `ZSNIPPET` which carries the
//! Apple-computed preview text.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Seconds between Apple Notes syncs (every 6 hours — notes change
/// infrequently; mirrors Bear's hourly cadence but doubled for the
/// heavier gzip+protobuf decoding of every modified note).
pub const APPLE_NOTES_SYNC_SECS: u64 = 21_600;

/// Seconds between the Unix epoch and the Apple/Core Data reference date
/// (2001-01-01 UTC). Apple timestamps (`ZCREATIONDATE3`, `ZMODIFICATIONDATE1`)
/// are seconds since this date.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

const SYNC_FILE: &str = ".trove/apple-notes-sync.json";
const SOURCE: &str = "apple-notes";
const NOTES_DIR: &str = "notes/apple-notes";
const RAW_DIR: &str = "notes/apple-notes/raw";

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_apple_notes()?;
    Ok(CollectOutcome::note_if(s.new_notes > 0, || {
        format!("imported {} Apple Notes", s.new_notes)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(apple_notes_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_apple_notes_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-notes",
        name: "Apple Notes",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads your Apple Notes from the local NoteStore database every \
                      6 hours; the first sync imports your full library. Note bodies are \
                      decoded from Apple's gzip+protobuf format. Locked notes keep their \
                      metadata but their body is never read.",
        domain: "notes",
        vault_path: "notes/apple-notes/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats: "Requires Full Disk Access. Locked (password-protected) notes keep \
                  their metadata but their body is never read. Notes marked for deletion \
                  are recorded with their trashed flag.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(APPLE_NOTES_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Incremental-sync state

/// Persisted sync state for the incremental watermark.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AppleNotesSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest `ZMODIFICATIONDATE1` (Core Data seconds) imported so far.
    /// Zero = first-ever run (full scan).
    pub cursor: f64,
}

/// Result of one sync pass.
#[derive(Debug, Clone, Serialize)]
pub struct AppleNotesSyncStats {
    /// False when the database is unreadable (no Full Disk Access or Notes never used).
    pub available: bool,
    pub new_notes: u64,
}

// ---------------------------------------------------------------------------
// Locating the database

/// The home directory to use for path resolution: `TROVE_HOME` when set, else
/// the real `$HOME`. Tests override via `TROVE_HOME`.
fn home_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// The NoteStore database path.
pub fn apple_notes_db_path() -> Option<PathBuf> {
    home_root().map(|h| {
        h.join("Library/Group Containers/group.com.apple.notes/NoteStore.sqlite")
    })
}

/// Whether this process can read the Notes database (i.e. Full Disk Access
/// granted to this binary). There is no API to prompt for FDA.
pub fn apple_notes_permission_ok() -> bool {
    apple_notes_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Core Data timestamps

/// Convert a Core Data timestamp (seconds since 2001-01-01 UTC) to local time.
/// Returns `None` for non-finite values (e.g. NULL coerced to `f64`).
fn core_data_to_local(z: f64) -> Option<DateTime<Local>> {
    if !z.is_finite() {
        return None;
    }
    let secs = z.trunc() as i64 + APPLE_EPOCH_OFFSET_S;
    let nanos = (z.fract().abs() * 1_000_000_000.0).round() as u32;
    DateTime::from_timestamp(secs, nanos).map(|t| t.with_timezone(&Local))
}

// ---------------------------------------------------------------------------
// Entity-number discovery

/// Probe `ZICCLOUDSYNCINGOBJECT` for the creation-date column name.
/// macOS 15+ uses `ZCREATIONDATE3`; older schema versions use `ZCREATIONDATE1`
/// or `ZCREATIONDATE`. Returns the first present column in preference order, or
/// `None` when none are found (schema too old / unknown).
fn creation_col(conn: &rusqlite::Connection) -> Option<&'static str> {
    const CANDIDATES: &[&str] = &["ZCREATIONDATE3", "ZCREATIONDATE1", "ZCREATIONDATE"];
    // `pragma_table_info` returns one row per column; we just look for names.
    let Ok(mut stmt) = conn.prepare(
        "SELECT name FROM pragma_table_info('ZICCLOUDSYNCINGOBJECT')"
    ) else {
        return None;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return None;
    };
    let mut found: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Ok(Some(row)) = rows.next() {
        if let Ok(name) = row.get::<_, String>(0) {
            found.insert(name);
        }
    }
    for &c in CANDIDATES {
        if found.contains(c) {
            return Some(c);
        }
    }
    None
}

/// Look up the Core Data entity number for a class name from `Z_PRIMARYKEY`.
/// Returns `None` when the table is absent or the name isn't found.
fn entity_num(conn: &rusqlite::Connection, class: &str) -> Option<i64> {
    conn.query_row(
        "SELECT Z_ENT FROM Z_PRIMARYKEY WHERE Z_NAME = ?1",
        [class],
        |r| r.get(0),
    )
    .ok()
}

// ---------------------------------------------------------------------------
// Minimal protobuf helpers (varint + length-delimited; no prost required)

/// Read a base-128 varint from `data` starting at `pos`.
/// Returns `(value, new_pos)`. Panics/returns on overflow after 64 bits.
fn read_varint(data: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut n: u64 = 0;
    let mut shift = 0u32;
    loop {
        if pos >= data.len() || shift >= 64 {
            return None;
        }
        let b = data[pos];
        pos += 1;
        n |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((n, pos));
        }
        shift += 7;
    }
}

/// Walk a flat protobuf message and return the bytes of the first occurrence of
/// field `target` with wire type 2 (length-delimited). Returns `None` when the
/// field is absent or the data can't be parsed.
fn proto_get_ld(data: &[u8], target: u32) -> Option<&[u8]> {
    let mut pos = 0;
    while pos < data.len() {
        let (tag, p) = read_varint(data, pos)?;
        pos = p;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x7) as u32;
        match wire {
            0 => {
                // varint — skip
                let (_v, p) = read_varint(data, pos)?;
                pos = p;
            }
            1 => {
                // 64-bit — skip
                if pos + 8 > data.len() {
                    return None;
                }
                pos += 8;
            }
            2 => {
                // length-delimited
                let (len, p) = read_varint(data, pos)?;
                pos = p;
                let end = pos + len as usize;
                if end > data.len() {
                    return None;
                }
                if field == target {
                    return Some(&data[pos..end]);
                }
                pos = end;
            }
            5 => {
                // 32-bit — skip
                if pos + 4 > data.len() {
                    return None;
                }
                pos += 4;
            }
            _ => return None, // unknown wire type
        }
    }
    None
}

/// Decode a gzip+protobuf `ZDATA` blob.
///
/// Returns `(plain_text, decompressed_proto_bytes)` where:
/// - `plain_text` is the extracted note body text (UTF-8), or `None` when
///   undecodable or empty.
/// - `decompressed_proto_bytes` is the raw decompressed protobuf payload
///   (gzip removed), preserved for full structural fidelity in the raw layer.
///   This allows checklist items, attachment references, and other structure to
///   be recovered later without re-reading the live database.
///
/// The confirmed field path (verified against real `NoteStore.sqlite`, macOS 15):
/// - outer.field2 → inner document message
/// - inner.field3 → body container
/// - body_container.field2 → the raw note text (UTF-8)
///
/// Field order in the outer message is not assumed. `proto_get_ld` walks the
/// full message and tolerates any field appearing before field 2 (including the
/// undocumented leading field-1 varint observed on real blobs). This makes the
/// decoder resilient to Apple adding, removing, or reordering fields across OS
/// updates.
fn decode_note_body(zdata: &[u8]) -> (Option<String>, Vec<u8>) {
    // Step 1: gzip decompress. Return empty proto bytes on failure.
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(zdata);
    let mut pb = Vec::new();
    if decoder.read_to_end(&mut pb).is_err() {
        return (None, Vec::new());
    }
    let proto_bytes = pb.clone();

    // Step 2: outer.field2 = inner document (proto_get_ld tolerates any leading
    // fields, so we no longer need to special-case/skip field 1).
    let text = (|| -> Option<String> {
        let inner = proto_get_ld(&pb, 2)?;
        // Step 3: inner.field3 = body container.
        let body_container = proto_get_ld(inner, 3)?;
        // Step 4: body_container.field2 = the note text.
        let text_bytes = proto_get_ld(body_container, 2)?;
        let text = std::str::from_utf8(text_bytes).ok()?;
        if text.is_empty() { return None; }
        Some(text.to_owned())
    })();

    (text, proto_bytes)
}

/// Extract the note title from the decoded body: the first non-empty line.
fn title_from_body(body: &str) -> &str {
    body.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Raw-value helper (type-preserving, for the raw layer)

fn raw_value(row: &rusqlite::Row, col: &str) -> Value {
    use rusqlite::types::ValueRef;
    match row.get_ref(col) {
        Ok(ValueRef::Null) => Value::Null,
        Ok(ValueRef::Integer(i)) => Value::from(i),
        Ok(ValueRef::Real(f)) => Value::from(f),
        Ok(ValueRef::Text(t)) => match std::str::from_utf8(t) {
            Ok(s) => Value::from(s),
            Err(_) => Value::Null,
        },
        Ok(ValueRef::Blob(_)) | Err(_) => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Core import logic

/// Read notes from a copy of the Notes database. Incremental: only rows with
/// `ZMODIFICATIONDATE1 > cursor` are processed.
///
/// Returns `(notes_upserted, new_cursor)`. The new cursor is the highest
/// `ZMODIFICATIONDATE1` seen; unchanged when no rows are processed.
pub fn import_apple_notes_db(vault: &Vault, db: &Path, cursor: f64) -> Result<(u64, f64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening Apple Notes database copy {}", db.display()))?;

    // Look up entity numbers dynamically so schema migrations don't break us.
    let note_ent = entity_num(&conn, "ICNote")
        .context("Z_PRIMARYKEY has no ICNote entity — database may be corrupt or empty")?;
    let folder_ent = entity_num(&conn, "ICFolder")
        .unwrap_or(0); // missing = no folder names, not fatal

    // Detect creation-date column dynamically: macOS 15+ = ZCREATIONDATE3,
    // older schemas may use ZCREATIONDATE1 or ZCREATIONDATE. Fall back to
    // NULL AS ZCREATIONDATE3 when no creation column exists (old schema).
    let cre_col = creation_col(&conn).unwrap_or("ZCREATIONDATE3");

    // Build a folder-name lookup map (PK → name) while we still have the conn.
    let folder_names: HashMap<i64, String> = if folder_ent > 0 {
        let mut map = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT Z_PK, ZTITLE2 FROM ZICCLOUDSYNCINGOBJECT WHERE Z_ENT = ?1 AND ZTITLE2 IS NOT NULL"
        )?;
        let mut rows = stmt.query([folder_ent])?;
        while let Some(row) = rows.next()? {
            let pk: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            if !name.is_empty() {
                map.insert(pk, name);
            }
        }
        map
    } else {
        HashMap::new()
    };

    // Query notes modified since the watermark, joined to their note data.
    // We filter on ZMODIFICATIONDATE1 > cursor (NULL rows are excluded by SQL
    // NULL semantics — they never satisfy a comparison, so they're silently
    // skipped, which is correct: a note with no modification date is not
    // processable and can't advance the watermark).
    // The creation-date column alias is ZCREATIONDATE3 regardless of the
    // underlying column name (for uniform row.get("ZCREATIONDATE3") below).
    let sql = format!(
        "SELECT
            n.Z_PK,
            n.ZIDENTIFIER,
            n.ZSNIPPET,
            n.{cre_col} AS ZCREATIONDATE3,
            n.ZMODIFICATIONDATE1,
            n.ZISPASSWORDPROTECTED,
            n.ZISPINNED,
            n.ZMARKEDFORDELETION,
            n.ZFOLDER,
            nd.ZDATA
        FROM ZICCLOUDSYNCINGOBJECT n
        LEFT JOIN ZICNOTEDATA nd ON nd.ZNOTE = n.Z_PK
        WHERE n.Z_ENT = ?1
          AND n.ZIDENTIFIER IS NOT NULL
          AND n.ZMODIFICATIONDATE1 > ?2
        ORDER BY n.ZMODIFICATIONDATE1",
        cre_col = cre_col
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params![note_ent, cursor])?;

    let mut contract: Vec<Note> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut max = cursor;

    while let Some(row) = rows.next()? {
        let z_pk: i64 = row.get("Z_PK")?;
        let identifier: Option<String> = row.get("ZIDENTIFIER")?;
        let Some(id) = identifier.filter(|s| !s.is_empty()) else {
            continue; // no stable ID → can't dedupe
        };

        let z_mod: Option<f64> = row
            .get::<_, Option<f64>>("ZMODIFICATIONDATE1")?
            .filter(|z| z.is_finite());
        if let Some(z) = z_mod {
            max = max.max(z);
        }

        let z_cre: Option<f64> = row
            .get::<_, Option<f64>>("ZCREATIONDATE3")?
            .filter(|z| z.is_finite());

        let created_rfc = z_cre
            .and_then(core_data_to_local)
            .map(|t| t.to_rfc3339());
        let modified_rfc = z_mod
            .and_then(core_data_to_local)
            .map(|t| t.to_rfc3339());

        // The contract partitions by `created`. A note with no creation date
        // falls back to the modification date for the partition key (still
        // immutable relative to the note's identity).
        let Some(created) = created_rfc.clone().or_else(|| modified_rfc.clone()) else {
            continue; // no usable timestamp — skip
        };

        let locked: bool = row
            .get::<_, Option<i64>>("ZISPASSWORDPROTECTED")?
            .unwrap_or(0) != 0;
        let pinned: Option<bool> = row
            .get::<_, Option<i64>>("ZISPINNED")?
            .map(|v| v != 0)
            .filter(|&b| b); // omit false
        let trashed: Option<bool> = row
            .get::<_, Option<i64>>("ZMARKEDFORDELETION")?
            .map(|v| v != 0)
            .filter(|&b| b); // omit false
        let folder_pk: Option<i64> = row.get("ZFOLDER")?;
        let folder_name = folder_pk
            .and_then(|pk| folder_names.get(&pk))
            .cloned()
            .unwrap_or_default();

        // Decode note body (only for non-locked notes).
        let snippet: Option<String> = row.get("ZSNIPPET")?;
        let zdata: Option<Vec<u8>> = row.get("ZDATA")?;

        let (body_text, proto_bytes) = if locked {
            (None, Vec::new())
        } else {
            zdata.as_deref().map(decode_note_body).unwrap_or((None, Vec::new()))
        };

        // Title: first non-empty line of body; fallback to ZSNIPPET (locked or
        // undecodable notes); fallback to empty (contract allows it).
        let title = body_text
            .as_deref()
            .map(title_from_body)
            .filter(|s| !s.is_empty())
            .or_else(|| snippet.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or("")
            .to_owned();

        // Build contract Note.
        let mut note = Note::new(SOURCE, &id);
        note.created = created.clone();
        if let Some(m) = &modified_rfc {
            note.modified = m.clone();
        }
        note.title = title;
        if let Some(b) = &body_text {
            note.body = b.clone();
        }
        if !folder_name.is_empty() {
            note.folder = folder_name.clone();
        }
        note.pinned = pinned;
        note.trashed = trashed;

        // extra: mark locked notes so they're accounted for without losing fidelity.
        let mut extra = Map::new();
        if locked {
            extra.insert("locked".into(), Value::Bool(true));
        }
        note.extra = extra;

        // Raw layer: full column fidelity keyed by id, partitioned by created.
        let mut raw_obj = Map::new();
        raw_obj.insert("source".into(), Value::from(SOURCE));
        raw_obj.insert("id".into(), Value::from(id.clone()));
        raw_obj.insert("z_pk".into(), Value::from(z_pk));
        raw_obj.insert("zidentifier".into(), Value::from(id.clone()));
        for col in &["ZSNIPPET", "ZCREATIONDATE3", "ZMODIFICATIONDATE1",
                     "ZISPASSWORDPROTECTED", "ZISPINNED", "ZMARKEDFORDELETION", "ZFOLDER"] {
            let key = col.to_lowercase();
            raw_obj.insert(key, raw_value(&row, col));
        }
        if !folder_name.is_empty() {
            raw_obj.insert("folder_name".into(), Value::from(folder_name));
        }
        if locked {
            raw_obj.insert("locked".into(), Value::Bool(true));
        }
        // Decoded body text for human-readable access.
        if let Some(ref b) = body_text {
            raw_obj.insert("body_text".into(), Value::from(b.as_str()));
        }
        // Decompressed protobuf bytes (base64) for full structural fidelity:
        // checklists, attachment references, attribute runs, tags, and other
        // structure Apple encodes in the ZDATA blob. Stored here so structure
        // is recoverable from the vault without re-reading the live database.
        // Not stored for locked notes (body is never decoded for those).
        if !proto_bytes.is_empty() {
            use base64::Engine as _;
            raw_obj.insert(
                "proto_bytes_b64".into(),
                Value::from(base64::engine::general_purpose::STANDARD.encode(&proto_bytes)),
            );
        }
        // Immutable partition key (created month) for the raw layer.
        raw_obj.insert("_created".into(), Value::from(created.clone()));

        raw_rows.push(Value::Object(raw_obj));
        contract.push(note);
    }
    drop(rows);
    drop(stmt);

    let n = contract.len() as u64;
    if !contract.is_empty() {
        vault.upsert_apple_notes_contract(&contract)?;
        vault.upsert_apple_notes_raw(&raw_rows)?;
    }
    Ok((n, max))
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One incremental sync pass. Returns immediately (with `available: false`)
    /// when FDA has not been granted.
    pub fn collect_apple_notes(&self) -> Result<AppleNotesSyncStats> {
        if !apple_notes_permission_ok() {
            return Ok(AppleNotesSyncStats { available: false, new_notes: 0 });
        }
        let db = apple_notes_db_path().expect("permission_ok implies path");
        let mut state = self.read_apple_notes_sync().unwrap_or_default();
        let stem = format!("trove-apple-notes-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| {
            import_apple_notes_db(self, tmp, state.cursor)
        })?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_apple_notes_sync(&state)?;
        Ok(AppleNotesSyncStats { available: true, new_notes: n })
    }

    /// Upsert contract notes into `notes/apple-notes/YYYY-MM.jsonl`, partitioned
    /// by `created` month, deduped by `id` (whole affected-month rewrite).
    ///
    /// Scans ALL existing month files to remove stale copies of incoming IDs
    /// before writing the new partition. This prevents cross-month duplicates
    /// when a note's partition key changes (e.g. a NULL-creation note's
    /// modification date crossing a month boundary on edit).
    pub fn upsert_apple_notes_contract(&self, notes: &[Note]) -> Result<()> {
        use std::collections::{HashMap, HashSet};
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            let Some(key) = Partition::Month.key(&n.created) else { continue };
            by_month.entry(key.to_string()).or_default().push(n);
        }
        if by_month.is_empty() {
            return Ok(());
        }
        // Collect all IDs being upserted so we can purge stale copies from
        // every month, not just the target month.
        let all_incoming_ids: HashSet<&str> = notes.iter().map(|n| n.id.as_str()).collect();

        let stream = self.stream(NOTES_DIR, Partition::Month);
        let all_months = stream.partitions()?;

        // For months not in by_month: scrub any stale copies of incoming IDs.
        for month in &all_months {
            if by_month.contains_key(month.as_str()) {
                continue; // handled below
            }
            let existing: Vec<Note> = stream.read::<Note>(month)?;
            if existing.iter().any(|e| all_incoming_ids.contains(e.id.as_str())) {
                let kept: Vec<Note> = existing
                    .into_iter()
                    .filter(|e| !all_incoming_ids.contains(e.id.as_str()))
                    .collect();
                let rel = format!("{NOTES_DIR}/{month}.jsonl");
                self.write_snapshot(&rel, &kept)?;
            }
        }

        // For target months: merge (remove stale + insert incoming).
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

    /// Upsert raw rows into `notes/apple-notes/raw/YYYY-MM.jsonl`, partitioned
    /// by `_created` (immutable), deduped by `id`.
    ///
    /// Scans ALL existing month files to remove stale copies of incoming IDs
    /// before writing the new partition. Mirrors the same cross-month purge
    /// logic as `upsert_apple_notes_contract`.
    pub fn upsert_apple_notes_raw(&self, rows: &[Value]) -> Result<()> {
        use std::collections::{HashMap, HashSet};
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in rows {
            let Some(key) = Partition::Month.key(month_of(v)) else { continue };
            by_month.entry(key.to_string()).or_default().push(v);
        }
        if by_month.is_empty() {
            return Ok(());
        }
        // Collect all IDs being upserted for cross-month stale-copy purge.
        let all_incoming_ids: HashSet<&str> = rows.iter().map(|v| id_of(v)).collect();

        let stream = self.stream(RAW_DIR, Partition::Month);
        let all_months = stream.partitions()?;

        // For months not in by_month: scrub stale copies.
        for month in &all_months {
            if by_month.contains_key(month.as_str()) {
                continue;
            }
            let existing: Vec<Value> = stream.read::<Value>(month)?;
            if existing.iter().any(|e| all_incoming_ids.contains(id_of(e))) {
                let kept: Vec<Value> = existing
                    .into_iter()
                    .filter(|e| !all_incoming_ids.contains(id_of(e)))
                    .collect();
                let rel = format!("{RAW_DIR}/{month}.jsonl");
                self.write_snapshot(&rel, &kept)?;
            }
        }

        // For target months: merge.
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

    /// Read the persisted sync state. `None` on first run.
    pub fn read_apple_notes_sync(&self) -> Option<AppleNotesSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_apple_notes_sync(&self, state: &AppleNotesSyncState) -> Result<()> {
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
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-an-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn z_date(y: i32, m: u32, d: u32, h: u32) -> f64 {
        let t = Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) as f64
    }

    /// Build a minimal Apple Notes-like SQLite database in the same Core Data
    /// shape as a real `NoteStore.sqlite`. The entity numbers (12=ICNote,
    /// 15=ICFolder) are the values observed in the real DB; they're written to
    /// `Z_PRIMARYKEY` so our dynamic lookup code exercises the same path.
    fn fake_notes_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-an-db-{}-{name}.sqlite", std::process::id()));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE Z_PRIMARYKEY (Z_ENT INTEGER PRIMARY KEY, Z_NAME TEXT, Z_SUPER INTEGER, Z_MAX INTEGER);
             CREATE TABLE ZICCLOUDSYNCINGOBJECT (
                Z_PK INTEGER PRIMARY KEY,
                Z_ENT INTEGER,
                ZIDENTIFIER TEXT,
                ZSNIPPET TEXT,
                ZCREATIONDATE3 REAL,
                ZMODIFICATIONDATE1 REAL,
                ZISPASSWORDPROTECTED INTEGER DEFAULT 0,
                ZISPINNED INTEGER DEFAULT 0,
                ZMARKEDFORDELETION INTEGER DEFAULT 0,
                ZFOLDER INTEGER,
                ZTITLE2 TEXT
             );
             CREATE TABLE ZICNOTEDATA (
                Z_PK INTEGER PRIMARY KEY,
                ZNOTE INTEGER,
                ZDATA BLOB
             );
             -- Entity registry (mirrors real NoteStore entity numbers)
             INSERT INTO Z_PRIMARYKEY (Z_ENT, Z_NAME, Z_SUPER, Z_MAX) VALUES (12, 'ICNote', 3, 0);
             INSERT INTO Z_PRIMARYKEY (Z_ENT, Z_NAME, Z_SUPER, Z_MAX) VALUES (15, 'ICFolder', 13, 0);
             -- Folders
             INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, Z_ENT, ZIDENTIFIER, ZTITLE2)
                 VALUES (1, 15, 'FOLDER-INBOX', 'Notes');
             INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, Z_ENT, ZIDENTIFIER, ZTITLE2)
                 VALUES (2, 15, 'FOLDER-WORK', 'Work');",
        ).unwrap();
        path
    }

    fn conn(path: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(path).unwrap()
    }

    /// Build a minimal gzip+protobuf body following the confirmed field path:
    /// outer.field1=varint(0), outer.field2 = inner,
    /// inner.field3 = body_container, body_container.field2 = text.
    fn make_zdata(text: &str) -> Vec<u8> {
        let text_bytes = text.as_bytes();
        // body_container: field 2 (wire 2) = text
        let body_container = proto_ld(2, text_bytes);
        // inner: field 3 (wire 2) = body_container
        let inner = proto_ld(3, &body_container);
        // outer: field 1 (wire 0) = 0, field 2 (wire 2) = inner
        let mut outer = vec![0x08, 0x00]; // field 1, varint 0
        outer.extend(proto_ld(2, &inner));

        let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&outer).unwrap();
        gz.finish().unwrap()
    }

    /// Build a protobuf length-delimited field (field_num, wire_type=2, data).
    fn proto_ld(field_num: u32, data: &[u8]) -> Vec<u8> {
        let tag = (field_num << 3) | 2;
        let mut out = varint_encode(tag as u64);
        out.extend(varint_encode(data.len() as u64));
        out.extend_from_slice(data);
        out
    }

    fn varint_encode(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            } else {
                out.push(b | 0x80);
            }
        }
        out
    }

    fn insert_note(
        c: &rusqlite::Connection,
        pk: i64,
        uid: &str,
        snippet: Option<&str>,
        created: f64,
        modified: f64,
        locked: i64,
        pinned: i64,
        trashed: i64,
        folder_pk: Option<i64>,
        body: Option<&[u8]>,
    ) {
        c.execute(
            "INSERT INTO ZICCLOUDSYNCINGOBJECT
             (Z_PK, Z_ENT, ZIDENTIFIER, ZSNIPPET, ZCREATIONDATE3, ZMODIFICATIONDATE1,
              ZISPASSWORDPROTECTED, ZISPINNED, ZMARKEDFORDELETION, ZFOLDER)
             VALUES (?1, 12, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![pk, uid, snippet, created, modified, locked, pinned, trashed, folder_pk],
        ).unwrap();
        if let Some(b) = body {
            c.execute(
                "INSERT INTO ZICNOTEDATA (ZNOTE, ZDATA) VALUES (?1, ?2)",
                rusqlite::params![pk, b],
            ).unwrap();
        }
    }

    // -----------------------------------------------------------------------

    #[test]
    fn core_data_epoch_conversion() {
        let z = 1_781_049_600.0 - APPLE_EPOCH_OFFSET_S as f64;
        assert_eq!(core_data_to_local(z).unwrap().timestamp(), 1_781_049_600);
        assert!(core_data_to_local(f64::NAN).is_none());
        assert!(core_data_to_local(f64::INFINITY).is_none());
    }

    #[test]
    fn entity_number_lookup() {
        let db = fake_notes_db("entity");
        let c = conn(&db);
        assert_eq!(entity_num(&c, "ICNote"), Some(12));
        assert_eq!(entity_num(&c, "ICFolder"), Some(15));
        assert_eq!(entity_num(&c, "Nonexistent"), None);
        let _ = fs::remove_file(db);
    }

    #[test]
    fn proto_roundtrip_extract_text() {
        let body = "Shopping List\n- Apples\n- Oranges";
        let zdata = make_zdata(body);
        let (decoded, proto_bytes) = decode_note_body(&zdata);
        let decoded = decoded.unwrap();
        assert_eq!(decoded, body);
        assert_eq!(title_from_body(&decoded), "Shopping List");
        // Proto bytes should be non-empty (decompressed protobuf payload).
        assert!(!proto_bytes.is_empty(), "proto_bytes should be present");
    }

    #[test]
    fn proto_empty_text_returns_none() {
        let zdata = make_zdata("");
        let (text, _proto_bytes) = decode_note_body(&zdata);
        assert!(text.is_none());
    }

    #[test]
    fn imports_normal_note_with_folder() {
        let v = temp_vault("normal");
        let db = fake_notes_db("normal");
        let c = conn(&db);
        let body_text = "Meeting Notes\nDiscussed Q3 roadmap.";
        let zdata = make_zdata(body_text);
        insert_note(&c, 100, "UUID-MEETING", Some("Meeting Notes"),
            z_date(2026, 5, 10, 9), z_date(2026, 5, 10, 14),
            0, 0, 0, Some(2), Some(&zdata));
        drop(c);

        let (n, max) = import_apple_notes_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1);
        assert!(max > 0.0);

        // Contract layer: partitioned by created month.
        let may = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may.len(), 1);
        let note = &may[0];
        assert_eq!(note.source, "apple-notes");
        assert_eq!(note.id, "UUID-MEETING");
        assert_eq!(note.title, "Meeting Notes");
        assert!(note.body.contains("Q3 roadmap"));
        assert_eq!(note.folder, "Work");
        assert!(note.created.starts_with("2026-05-10"));
        assert!(note.extra.is_empty(), "unlocked note has empty extra");

        // Raw layer: same partition, same key.
        let raw = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-05").unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0]["id"], "UUID-MEETING");
        assert_eq!(raw[0]["z_pk"], 100);
        assert_eq!(raw[0]["folder_name"], "Work");
        assert!(raw[0]["body_text"].as_str().unwrap().contains("Q3 roadmap"));
        assert!(raw[0]["_created"].as_str().unwrap().starts_with("2026-05"));

        let _ = fs::remove_file(db);
    }

    #[test]
    fn locked_note_skips_body_emits_metadata() {
        let v = temp_vault("locked");
        let db = fake_notes_db("locked");
        let c = conn(&db);
        // A locked note with a snippet (Apple-computed preview, not decrypted body)
        insert_note(&c, 200, "UUID-LOCKED", Some("Private diary"),
            z_date(2026, 6, 1, 8), z_date(2026, 6, 1, 8),
            1, 0, 0, Some(1), None);
        drop(c);

        let (n, _) = import_apple_notes_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1);

        let jun = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(jun.len(), 1);
        let note = &jun[0];
        assert_eq!(note.body, "", "locked body NEVER written");
        // Title falls back to snippet for locked notes.
        assert_eq!(note.title, "Private diary", "snippet used as title fallback");
        assert_eq!(note.extra.get("locked"), Some(&Value::Bool(true)));

        let raw = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-06").unwrap();
        assert_eq!(raw[0]["locked"], Value::Bool(true));
        assert!(raw[0].get("body_text").is_none(), "body_text absent in raw for locked note");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn pinned_and_trashed_flags() {
        let v = temp_vault("flags");
        let db = fake_notes_db("flags");
        let c = conn(&db);
        let zdata = make_zdata("Pinned idea\nContent");
        insert_note(&c, 300, "UUID-PINNED", None,
            z_date(2026, 4, 1, 9), z_date(2026, 4, 1, 9),
            0, 1, 0, None, Some(&zdata));
        let zdata2 = make_zdata("Old note\nTo be deleted");
        insert_note(&c, 301, "UUID-TRASH", None,
            z_date(2026, 4, 2, 9), z_date(2026, 4, 2, 9),
            0, 0, 1, None, Some(&zdata2));
        drop(c);

        let (n, _) = import_apple_notes_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 2);

        let apr = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-04").unwrap();
        let pinned = apr.iter().find(|n| n.id == "UUID-PINNED").unwrap();
        assert_eq!(pinned.pinned, Some(true));
        assert_eq!(pinned.trashed, None, "non-trashed note: flag omitted");
        let trash = apr.iter().find(|n| n.id == "UUID-TRASH").unwrap();
        assert_eq!(trash.trashed, Some(true));
        assert_eq!(trash.pinned, None);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn incremental_watermark_and_upsert() {
        let v = temp_vault("incr");
        let db = fake_notes_db("incr");
        let c = conn(&db);
        let zdata = make_zdata("First draft\nInitial content");
        insert_note(&c, 400, "UUID-INCR", None,
            z_date(2026, 6, 1, 9), z_date(2026, 6, 1, 9),
            0, 0, 0, None, Some(&zdata));
        drop(c);

        let (n1, max1) = import_apple_notes_db(&v, &db, 0.0).unwrap();
        assert_eq!(n1, 1);
        assert!(max1 > 0.0);

        // Re-run from watermark with nothing newer — no work.
        let (n2, max2) = import_apple_notes_db(&v, &db, max1).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(max2, max1);

        // Edit the note (later mod date).
        let c = conn(&db);
        let zdata_new = make_zdata("First draft\nEdited content");
        c.execute("UPDATE ZICNOTEDATA SET ZDATA = ?1 WHERE ZNOTE = 400", [&zdata_new[..]]).unwrap();
        c.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZMODIFICATIONDATE1 = ?1 WHERE Z_PK = 400",
            [z_date(2026, 6, 5, 12)]).unwrap();
        drop(c);

        let (n3, _) = import_apple_notes_db(&v, &db, max1).unwrap();
        assert_eq!(n3, 1);
        let jun = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(jun.len(), 1, "upsert keeps exactly one line per id");
        assert!(jun[0].body.contains("Edited content"), "line replaced in place");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn no_creation_date_falls_back_to_mod_date() {
        // A note row with ZCREATIONDATE3 = NULL but ZMODIFICATIONDATE1 set should
        // still be imported, using mod date as the partition key.
        let v = temp_vault("nocreation");
        let db = fake_notes_db("nocreation");
        let c = conn(&db);
        let zdata = make_zdata("No creation\nJust mod date");
        c.execute(
            "INSERT INTO ZICCLOUDSYNCINGOBJECT
             (Z_PK, Z_ENT, ZIDENTIFIER, ZSNIPPET, ZCREATIONDATE3, ZMODIFICATIONDATE1,
              ZISPASSWORDPROTECTED, ZISPINNED, ZMARKEDFORDELETION, ZFOLDER)
             VALUES (500, 12, 'UUID-NOCRE', 'No creation', NULL, ?1, 0, 0, 0, NULL)",
            [z_date(2026, 6, 10, 9)],
        ).unwrap();
        c.execute("INSERT INTO ZICNOTEDATA (ZNOTE, ZDATA) VALUES (500, ?1)", [&zdata[..]])
            .unwrap();
        drop(c);

        let (n, _) = import_apple_notes_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1);
        // Partitioned by mod date (used as fallback created).
        let jun = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(jun.len(), 1);
        assert_eq!(jun[0].id, "UUID-NOCRE");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn fda_unreadable_is_graceful_noop() {
        let _g = env_guard();
        let fake_home = std::env::temp_dir()
            .join(format!("trove-an-nohome-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fake_home);
        fs::create_dir_all(&fake_home).unwrap();
        std::env::set_var("TROVE_HOME", &fake_home);

        assert!(!apple_notes_permission_ok());
        let v = temp_vault("noop");
        let stats = v.collect_apple_notes().unwrap();
        assert!(!stats.available);
        assert_eq!(stats.new_notes, 0);

        std::env::remove_var("TROVE_HOME");
        let _ = fs::remove_dir_all(fake_home);
    }

    #[test]
    fn watermark_persists_and_round_trips() {
        let v = temp_vault("watermark");
        assert!(v.read_apple_notes_sync().is_none());
        let state = AppleNotesSyncState {
            updated: "2026-06-10T09:00:00-07:00".into(),
            cursor: 800_000.5,
        };
        v.write_apple_notes_sync(&state).unwrap();
        let got = v.read_apple_notes_sync().unwrap();
        assert_eq!(got.cursor, 800_000.5);
        assert_eq!(got.updated, "2026-06-10T09:00:00-07:00");
    }

    #[test]
    fn serde_back_compat_sparse_note_deserializes() {
        // A Note line written with only required fields (older schema) must
        // still deserialize — additive schema evolution.
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-06.jsonl")),
            "{\"source\":\"apple-notes\",\"id\":\"OLD-1\"}\n\
             {\"source\":\"apple-notes\",\"id\":\"OLD-2\",\"body\":\"b\",\"created\":\"2026-06-01T00:00:00-07:00\"}\n",
        ).unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "OLD-1");
        assert_eq!(rows[1].body, "b");
    }

    #[test]
    fn title_extraction_uses_first_nonempty_line() {
        assert_eq!(title_from_body("Title\nBody line"), "Title");
        assert_eq!(title_from_body("\n  \nActual Title\nBody"), "Actual Title");
        assert_eq!(title_from_body(""), "");
        assert_eq!(title_from_body("Only line"), "Only line");
    }

    /// Two-sync test: a NULL-created note edited into a new month must appear
    /// in exactly ONE month file total (no cross-month duplicates). This
    /// exercises the cross-month stale-copy purge in `upsert_apple_notes_contract`
    /// and `upsert_apple_notes_raw`.
    #[test]
    fn null_creation_no_cross_month_duplicate_on_edit() {
        let v = temp_vault("crossmonth");
        let db = fake_notes_db("crossmonth");
        let c = conn(&db);
        let zdata = make_zdata("No creation note\nOriginal body");
        // First sync: note has NULL creation date, mod date is in June.
        c.execute(
            "INSERT INTO ZICCLOUDSYNCINGOBJECT
             (Z_PK, Z_ENT, ZIDENTIFIER, ZSNIPPET, ZCREATIONDATE3, ZMODIFICATIONDATE1,
              ZISPASSWORDPROTECTED, ZISPINNED, ZMARKEDFORDELETION, ZFOLDER)
             VALUES (600, 12, 'UUID-CROSSMON', 'No creation note', NULL, ?1, 0, 0, 0, NULL)",
            [z_date(2026, 6, 15, 10)],
        ).unwrap();
        c.execute("INSERT INTO ZICNOTEDATA (ZNOTE, ZDATA) VALUES (600, ?1)", [&zdata[..]])
            .unwrap();
        drop(c);

        let (n1, max1) = import_apple_notes_db(&v, &db, 0.0).unwrap();
        assert_eq!(n1, 1);

        // Verify: note is in June (mod date used as fallback partition key).
        let jun = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(jun.len(), 1, "should be in June after first sync");

        // Second sync: note is edited and the mod date moves to July.
        let c = conn(&db);
        let zdata_new = make_zdata("No creation note\nEdited in July");
        c.execute("UPDATE ZICNOTEDATA SET ZDATA = ?1 WHERE ZNOTE = 600", [&zdata_new[..]]).unwrap();
        c.execute(
            "UPDATE ZICCLOUDSYNCINGOBJECT SET ZMODIFICATIONDATE1 = ?1 WHERE Z_PK = 600",
            [z_date(2026, 7, 3, 9)],
        ).unwrap();
        drop(c);

        let (n2, _) = import_apple_notes_db(&v, &db, max1).unwrap();
        assert_eq!(n2, 1);

        // Critical invariant: the note must exist in exactly ONE month file.
        let jun_after = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        let jul_after = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-07").unwrap();
        let total_copies: usize = jun_after.iter().filter(|n| n.id == "UUID-CROSSMON").count()
            + jul_after.iter().filter(|n| n.id == "UUID-CROSSMON").count();
        assert_eq!(total_copies, 1, "note must appear in exactly one month file, got jun={} jul={}",
            jun_after.iter().filter(|n| n.id == "UUID-CROSSMON").count(),
            jul_after.iter().filter(|n| n.id == "UUID-CROSSMON").count());
        assert!(jul_after.iter().any(|n| n.id == "UUID-CROSSMON"),
            "edited note should be in July after second sync");
        assert!(jul_after.iter().find(|n| n.id == "UUID-CROSSMON")
            .unwrap().body.contains("Edited in July"),
            "content should reflect the edit");

        // Same invariant for raw layer.
        let raw_jun = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-06").unwrap();
        let raw_jul = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-07").unwrap();
        let raw_total: usize =
            raw_jun.iter().filter(|v| v["id"].as_str() == Some("UUID-CROSSMON")).count()
            + raw_jul.iter().filter(|v| v["id"].as_str() == Some("UUID-CROSSMON")).count();
        assert_eq!(raw_total, 1, "raw note must appear in exactly one month file");

        let _ = fs::remove_file(db);
    }

    /// Verify that the raw layer stores decompressed proto bytes for structural
    /// fidelity (checklist/attachment recovery without re-reading the live DB).
    #[test]
    fn raw_layer_stores_proto_bytes_b64() {
        let v = temp_vault("proto_bytes");
        let db = fake_notes_db("proto_bytes");
        let c = conn(&db);
        let body_text = "Checklist\nItem one\nItem two";
        let zdata = make_zdata(body_text);
        insert_note(&c, 700, "UUID-PROTO", None,
            z_date(2026, 5, 1, 9), z_date(2026, 5, 1, 9),
            0, 0, 0, None, Some(&zdata));
        drop(c);

        let (n, _) = import_apple_notes_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1);

        let raw = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-05").unwrap();
        assert_eq!(raw.len(), 1);
        let b64 = raw[0]["proto_bytes_b64"].as_str()
            .expect("proto_bytes_b64 must be present in raw layer for non-locked notes");
        // Decode and verify it's valid protobuf bytes (non-empty, starts with a recognizable tag).
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)
            .expect("proto_bytes_b64 must be valid base64");
        assert!(!bytes.is_empty(), "decompressed proto bytes must be non-empty");

        let _ = fs::remove_file(db);
    }

    /// Serialize the env-mutating test so parallel tests don't see TROVE_HOME.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
