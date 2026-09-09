//! Bear notes collector — an M3 ("copy-then-read another app's SQLite")
//! source over Bear's Core Data store, modeled on
//! [`crate::apple_voice_memos`]. Needs **Full Disk Access** (the same
//! per-binary, no-programmatic-prompt grant as Messages / Voice Memos /
//! Safari history); silently skipped while the database is unreadable.
//!
//! Writes the [`notes`](crate::notes) domain: a normalized **contract** layer
//! `notes/bear/YYYY-MM.jsonl` (one [`Note`] per note, partitioned by the local
//! month of `created`, deduped/upserted by `id` via a per-affected-month
//! whole-file rewrite) plus a full-fidelity **raw** layer
//! `notes/bear/raw/YYYY-MM.jsonl` (every selected `ZSFNOTE` column verbatim,
//! partitioned by the month of `modified` — the brief's split). The raw layer
//! is the lossless record; the contract layer is what the notes reader scans.
//!
//! **Schema (community-documented):** Bear stores its notes in
//! `~/Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application
//! Data/database.sqlite`, a Core Data store. Notes live in `ZSFNOTE`; the
//! columns we read are `ZUNIQUEIDENTIFIER` (the stable note UUID — the dedupe
//! `id`, NOT `Z_PK`), `ZTITLE`, `ZTEXT` (the Markdown body), `ZCREATIONDATE` /
//! `ZMODIFICATIONDATE` (Core Data seconds since 2001), `ZTRASHED` /
//! `ZARCHIVED` / `ZPINNED` / `ZENCRYPTED` (integer 0/1 flags), and `Z_PK` (the
//! integer primary key, used only to join tags). The schema varies across Bear
//! versions, so the `SELECT` is built defensively from `PRAGMA table_info` — a
//! column the local store lacks is simply skipped, never an error. Bear has no
//! folder model (organization is tag-only), so the contract's `folder` is
//! always omitted.
//!
//! **Tags (many-to-many):** tag names are `ZSFNOTETAG.ZTITLE`. Notes join to
//! tags through a Core Data join table named `Z_<N>TAGS` whose entity number
//! `<N>` is schema-version-dependent (`Z_5TAGS` and `Z_7TAGS` are both seen in
//! the wild), with two FK columns *also* numbered — one ending `…NOTES` (→
//! `ZSFNOTE.Z_PK`), one ending `…TAGS` (→ `ZSFNOTETAG.Z_PK`), e.g.
//! `Z_5TAGS(Z_5NOTES, Z_13TAGS)`. We **discover** the join table and its
//! columns dynamically (scan `sqlite_master` for the `Z_%TAGS` table, PRAGMA
//! its columns, pick the `…NOTES`/`…TAGS` pair) rather than hardcoding the
//! number — exactly the schema-adaptive discipline the PRAGMA column select
//! uses. Confirmed against the community Bear parsers (`bart6114/bear-mcp`,
//! `andymatuschak/Bear-Markdown-Export`, `vasylenko/bear-notes-mcp`).
//!
//! **Encrypted notes (`ZENCRYPTED=1`):** the body is an opaque encrypted blob
//! — we **never** write ciphertext as a plaintext `body`. The metadata row is
//! still emitted (id, created, modified, flags, and the title only when it's
//! valid plaintext) with `extra.encrypted=true`, so the note is accounted for
//! without leaking ciphertext. Defensive throughout: a column read that isn't
//! valid UTF-8 / is an unexpected blob is dropped, not written.
//!
//! Incremental sync watermarks on the **highest `ZMODIFICATIONDATE`** seen, in
//! `.trove/bear-sync.json` (non-secret, rebuildable by re-scanning the DB).
//! First run (no watermark) is a full scan. Dedupe is by `id` (a whole rewrite
//! of each affected `created`-month file), so a lost watermark or a clock that
//! moved backward never duplicates a note. **Known v1 limitation:** a note the
//! user *hard-deletes* in Bear (row removed, not trashed) can't be detected by
//! a modification-watermark scan — its line lingers until a full rescan; a
//! *trashed* note (kept with `ZTRASHED=1`) IS captured.

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

/// Seconds between Bear syncs in the watcher loop (hourly — notes change
/// infrequently relative to messages; mirrors Voice Memos).
pub const BEAR_SYNC_SECS: u64 = 3600;

/// Seconds between the Unix epoch and the Apple/Core Data epoch (2001-01-01,
/// UTC). `ZCREATIONDATE` / `ZMODIFICATIONDATE` are seconds since then.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

const SYNC_FILE: &str = ".trove/bear-sync.json";
const SOURCE: &str = "bear";
const NOTES_DIR: &str = "notes/bear";
const RAW_DIR: &str = "notes/bear/raw";

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_bear()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_notes > 0, || {
        format!("imported {} Bear notes", s.new_notes)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(bear_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_bear_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bear",
        name: "Bear",
        kind: IntegrationKind::LocalSync,
        // Opt-in: a user's own notes are personal content (the 🔒 gate the hub
        // renders for default-off integrations).
        default_on: false,
        description: "Reads your Bear notes and tags from Bear's local database \
                      every hour; the first sync backfills your whole library. \
                      Encrypted notes keep their metadata but their locked body is \
                      never read.",
        domain: "notes",
        vault_path: "notes/bear/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats: "Requires Full Disk Access. Encrypted notes (locked in Bear) keep \
                  their metadata but their body is never read. A note hard-deleted in \
                  Bear lingers until a full rescan; a trashed note is captured with its \
                  trashed flag.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(BEAR_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Incremental-sync state, persisted in `.trove/bear-sync.json`. The watermark
/// is advisory only — dedupe is by `id` — so it never needs rebuilding for
/// correctness.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct BearSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest `ZMODIFICATIONDATE` (Core Data seconds) imported so far.
    pub cursor: f64,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Serialize)]
pub struct BearSyncStats {
    /// False when the database is unreadable (no Full Disk Access, or Bear
    /// never used) — nothing was attempted.
    pub available: bool,
    pub new_notes: u64,
}

// ---------------------------------------------------------------------------
// Locating the store (honors TROVE_HOME / HOME so tests use a temp tree).

/// The home dir to resolve Bear paths under: `TROVE_HOME` when set and
/// non-empty, else the real home dir. Mirrors [`crate::apple_voice_memos`].
fn home_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// Bear's group-container SQLite database (Bear 1 and Bear 2 share this path).
fn bear_db_path() -> Option<PathBuf> {
    home_root().map(|h| {
        h.join("Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application Data/database.sqlite")
    })
}

/// Whether this process can read the Bear database. False means Full Disk
/// Access hasn't been granted to this binary (or Bear has never run). There is
/// no API to prompt for FDA — the UI deep-links to System Settings, same as
/// Messages / Voice Memos.
pub fn bear_permission_ok() -> bool {
    bear_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Core Data date

/// `ZCREATIONDATE` / `ZMODIFICATIONDATE` (Core Data seconds since 2001, UTC;
/// may be fractional) → local. None if non-finite.
fn core_data_to_local(z: f64) -> Option<DateTime<Local>> {
    if !z.is_finite() {
        return None;
    }
    let secs = z.trunc() as i64 + APPLE_EPOCH_OFFSET_S;
    let nanos = (z.fract().abs() * 1_000_000_000.0).round() as u32;
    DateTime::from_timestamp(secs, nanos).map(|t| t.with_timezone(&Local))
}

// ---------------------------------------------------------------------------
// Schema-adaptive column probe (ZSFNOTE varies across Bear versions)

/// Which optional `ZSFNOTE` columns this store carries. `Z_PK`,
/// `ZUNIQUEIDENTIFIER`, `ZTEXT`, `ZCREATIONDATE`, `ZMODIFICATIONDATE` are the
/// core we always try to read; these flags/fields are version-dependent.
struct Columns {
    title: bool,
    trashed: bool,
    archived: bool,
    pinned: bool,
    encrypted: bool,
    /// Every column name present, uppercased — so the raw layer can dump the
    /// full row (full fidelity) regardless of which we map.
    all: Vec<String>,
}

fn probe_columns(conn: &rusqlite::Connection) -> Result<Columns> {
    let mut all = Vec::new();
    let mut have = HashSet::new();
    let mut stmt = conn.prepare("PRAGMA table_info(ZSFNOTE)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        let up = name.to_uppercase();
        have.insert(up.clone());
        all.push(up);
    }
    Ok(Columns {
        title: have.contains("ZTITLE"),
        trashed: have.contains("ZTRASHED"),
        archived: have.contains("ZARCHIVED"),
        pinned: have.contains("ZPINNED"),
        encrypted: have.contains("ZENCRYPTED"),
        all,
    })
}

/// The discovered note↔tag join: the join table name and which of its two
/// columns references notes (`ZSFNOTE.Z_PK`) vs tags (`ZSFNOTETAG.Z_PK`).
struct TagJoin {
    table: String,
    notes_col: String,
    tags_col: String,
}

/// Discover the `Z_<N>TAGS` join table and its FK columns dynamically. The
/// entity number `<N>` is schema-version-dependent (`Z_5TAGS`, `Z_7TAGS`, …),
/// and the two FK columns are themselves numbered — so we never hardcode them:
/// find the table whose name matches `Z_%TAGS`, PRAGMA its columns, and pick
/// the one ending `NOTES` (→ notes) and the one ending `TAGS` (→ tags).
/// `None` (no tags joined) when the store has no such table — older/empty
/// libraries, or a future schema that renamed it; tags are simply omitted then.
fn discover_tag_join(conn: &rusqlite::Connection) -> Result<Option<TagJoin>> {
    // Match the Core Data many-to-many join table for tags. `ZSFNOTETAG` is
    // the entity table (excluded by the `Z_%` underscore-numbered convention);
    // the join table is `Z_<digits>TAGS`.
    // Assumption: real Bear has exactly ONE such table, so `ORDER BY name LIMIT
    // 1` is deterministic — there is no second `Z_%TAGS` to disambiguate from.
    let table: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name LIKE 'Z\\_%TAGS' ESCAPE '\\' \
               AND name NOT LIKE 'ZSF%' \
             ORDER BY name LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let Some(table) = table else {
        return Ok(None);
    };

    let mut notes_col = None;
    let mut tags_col = None;
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        let up = name.to_uppercase();
        if up.ends_with("NOTES") {
            notes_col = Some(name);
        } else if up.ends_with("TAGS") {
            tags_col = Some(name);
        }
    }
    match (notes_col, tags_col) {
        (Some(notes_col), Some(tags_col)) => Ok(Some(TagJoin { table, notes_col, tags_col })),
        // A join table whose columns don't fit the pattern: treat as no tags
        // rather than guess — the raw layer still preserves everything.
        _ => Ok(None),
    }
}

/// Tags per note `Z_PK`, in tag-title order, built from one pass over the join
/// table ⨝ `ZSFNOTETAG`. Empty map when there is no join table.
fn load_tags(conn: &rusqlite::Connection) -> Result<HashMap<i64, Vec<String>>> {
    let Some(join) = discover_tag_join(conn)? else {
        return Ok(HashMap::new());
    };
    let sql = format!(
        "SELECT j.\"{notes}\" AS note_pk, t.ZTITLE AS tag \
         FROM \"{table}\" j JOIN ZSFNOTETAG t ON t.Z_PK = j.\"{tags}\" \
         WHERE t.ZTITLE IS NOT NULL \
         ORDER BY note_pk, t.ZTITLE",
        table = join.table,
        notes = join.notes_col,
        tags = join.tags_col,
    );
    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let note_pk: i64 = row.get("note_pk")?;
        let tag: String = row.get::<_, Option<String>>("tag")?.unwrap_or_default();
        if !tag.is_empty() {
            map.entry(note_pk).or_default().push(tag);
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// the import

/// Read a flag column as a bool: `Some(true)` only when the integer is
/// non-zero. A missing/NULL/garbage value → `None` (omit the flag).
fn flag(row: &rusqlite::Row, col: &str) -> Option<bool> {
    let v: Option<i64> = row.get::<_, Option<i64>>(col).ok().flatten();
    v.map(|n| n != 0)
}

/// Read a column as a non-empty plaintext string, defensively: only when it's
/// valid text (rusqlite yields `Err`/`None` for a blob or NULL). Empty → None.
fn text(row: &rusqlite::Row, col: &str) -> Option<String> {
    row.get::<_, Option<String>>(col).ok().flatten().filter(|s| !s.is_empty())
}

/// Read notes out of a Bear database (or a copy) and upsert, by `id`, every
/// note into both the contract layer (`notes/bear/YYYY-MM.jsonl`, by `created`
/// month) and the raw layer (`notes/bear/raw/YYYY-MM.jsonl`, by `modified`
/// month). Incremental: only rows whose `ZMODIFICATIONDATE` exceeds `cursor`
/// are read. Returns (notes upserted, new watermark = highest mod-date seen).
/// Split from the copy/locate steps so tests run it on a synthetic DB.
fn import_bear_db(vault: &Vault, db: &Path, cursor: f64) -> Result<(u64, f64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening Bear database copy {}", db.display()))?;
    let cols = probe_columns(&conn)?;
    let tags_by_pk = load_tags(&conn)?;

    // Build the SELECT from the columns that exist. The core five are assumed
    // present; the flags/title are optional. `*` is not used — we name columns
    // so a renamed-away one degrades to "missing" rather than shifting indices.
    let mut select: Vec<&str> =
        vec!["Z_PK", "ZUNIQUEIDENTIFIER", "ZTEXT", "ZCREATIONDATE", "ZMODIFICATIONDATE"];
    if cols.title {
        select.push("ZTITLE");
    }
    if cols.trashed {
        select.push("ZTRASHED");
    }
    if cols.archived {
        select.push("ZARCHIVED");
    }
    if cols.pinned {
        select.push("ZPINNED");
    }
    if cols.encrypted {
        select.push("ZENCRYPTED");
    }
    let sql = format!(
        "SELECT {} FROM ZSFNOTE WHERE ZMODIFICATIONDATE > ?1 ORDER BY ZMODIFICATIONDATE",
        select.join(", ")
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([cursor])?;
    let mut contract: Vec<Note> = Vec::new();
    let mut raw: Vec<Value> = Vec::new();
    let mut max = cursor;

    while let Some(row) = rows.next()? {
        let z_pk: i64 = row.get("Z_PK")?;
        // A NULL/absent modification date is genuinely unknown — never coerce
        // it to 0.0 (that would fabricate a 2001-01-01 `modified` and a bogus
        // watermark). The query already filters on `ZMODIFICATIONDATE > cursor`,
        // so a NULL row is normally excluded; this stays defensive regardless.
        let z_mod: Option<f64> = row.get::<_, Option<f64>>("ZMODIFICATIONDATE")?.filter(|z| z.is_finite());
        if let Some(z) = z_mod {
            max = max.max(z);
        }

        // id: the stable note UUID. A row without one is unusable as a dedupe
        // key — skip it (the raw layer would have nothing to key on either).
        let Some(id) = text(&row, "ZUNIQUEIDENTIFIER") else {
            continue;
        };

        let modified = z_mod.and_then(core_data_to_local).map(|t| t.to_rfc3339());
        let created = row
            .get::<_, Option<f64>>("ZCREATIONDATE")?
            .and_then(core_data_to_local)
            .map(|t| t.to_rfc3339());
        // The contract partitions by `created`; a note with no creation date
        // can't be filed, so fall back to the modification date for the key.
        let Some(created) = created.or_else(|| modified.clone()) else {
            continue;
        };

        let encrypted = cols.encrypted && flag(&row, "ZENCRYPTED") == Some(true);

        let mut note = Note::new(SOURCE, &id);
        note.created = created;
        if let Some(m) = &modified {
            note.modified = m.clone();
        }
        // Title is plaintext even for encrypted notes in practice, but guard
        // it defensively (only emit valid non-empty text).
        if cols.title {
            if let Some(t) = text(&row, "ZTITLE") {
                note.title = t;
            }
        }
        // Body: the opaque blob of an encrypted note is NEVER written as
        // plaintext. For a normal note, write the full Markdown verbatim.
        if !encrypted {
            if let Some(b) = text(&row, "ZTEXT") {
                note.body = b;
            }
        }
        if cols.trashed {
            note.trashed = flag(&row, "ZTRASHED").filter(|&t| t);
        }
        if cols.archived {
            note.archived = flag(&row, "ZARCHIVED").filter(|&a| a);
        }
        if cols.pinned {
            note.pinned = flag(&row, "ZPINNED").filter(|&p| p);
        }
        if let Some(tags) = tags_by_pk.get(&z_pk) {
            note.tags = tags.clone();
        }
        // Bear has no folder model (tags only) → `folder` stays omitted.

        // extra: the encrypted marker only. `Z_PK` is deliberately NOT carried
        // here — it's the Core Data rowid (reassigned on store rebuild), not
        // note data; persisting it would churn every contract line whenever Bear
        // renumbers, breaking the snapshot's "an unchanged note keeps its line"
        // guarantee. The RAW layer keeps `z_pk` for full fidelity.
        let mut extra = Map::new();
        if encrypted {
            extra.insert("encrypted".into(), Value::Bool(true));
        }
        note.extra = extra;

        // Raw layer: the full selected row, every column verbatim, keyed by id
        // and partitioned by **created** month. Lossless. The body of an
        // encrypted note is still withheld here (it's ciphertext, not data we
        // can faithfully represent as text) — `encrypted:true` records why.
        //
        // Why created, not modified: the raw layer upserts by `id` within a
        // month file, so the partition key must be immutable. `modified` moves
        // when a note is edited — a row keyed on it would land in a *new* month
        // file while its stale copy lingered in the old one (the same `id` in
        // two files with conflicting bodies). `created` never changes, so a
        // note's row stays in exactly one file and id-upsert is complete. The
        // real `zmodificationdate` is still preserved verbatim as a column.
        let mut raw_obj = Map::new();
        raw_obj.insert("source".into(), Value::from(SOURCE));
        raw_obj.insert("id".into(), Value::from(id.clone()));
        raw_obj.insert("z_pk".into(), Value::from(z_pk));
        for col in &cols.all {
            // Skip columns not in the SELECT (we only have the selected ones
            // bound) and the ciphertext body.
            if !select.iter().any(|s| s.eq_ignore_ascii_case(col)) {
                continue;
            }
            if col == "ZTEXT" && encrypted {
                continue;
            }
            let key = col.to_lowercase();
            let val = raw_value(&row, col);
            raw_obj.insert(key, val);
        }
        if encrypted {
            raw_obj.insert("encrypted".into(), Value::Bool(true));
        }
        // The immutable partition key for the raw stream (created month) —
        // mirrors the contract layer so a note's two rows live in the same month.
        raw_obj.insert("_created".into(), Value::from(note.created.clone()));
        if !note.tags.is_empty() {
            raw_obj.insert(
                "tags".into(),
                Value::Array(note.tags.iter().cloned().map(Value::from).collect()),
            );
        }
        raw.push(Value::Object(raw_obj));

        contract.push(note);
    }
    drop(rows);
    drop(stmt);

    let n = contract.len() as u64;
    if !contract.is_empty() {
        vault.upsert_bear_notes(&contract)?;
        vault.upsert_bear_raw(&raw)?;
    }
    Ok((n, max))
}

/// One selected column's value as JSON, type-preserving (int / float / text),
/// for the raw layer. A blob or NULL becomes `null`.
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
        // A blob is not faithfully representable as text — drop it.
        Ok(ValueRef::Blob(_)) | Err(_) => Value::Null,
    }
}

impl Vault {
    /// One incremental sync pass over the Bear database. Silently a no-op
    /// (with `available:false`) while the database is unreadable — the UI
    /// surfaces the permission state; logging every pass would be noise.
    pub fn collect_bear(&self) -> Result<BearSyncStats> {
        if !bear_permission_ok() {
            return Ok(BearSyncStats { available: false, new_notes: 0 });
        }
        let db = bear_db_path().expect("permission_ok implies path");
        let mut state = self.read_bear_sync().unwrap_or_default();
        let stem = format!("trove-bear-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| import_bear_db(self, tmp, state.cursor))?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_bear_sync(&state)?;
        Ok(BearSyncStats { available: true, new_notes: n })
    }

    /// Upsert notes into `notes/bear/YYYY-MM.jsonl` (the contract layer),
    /// partitioned by the month of `created`, deduped by `id`: each affected
    /// month file is read, the fresh notes replace any same-`id` lines, and the
    /// file is rewritten atomically. A note whose `created` month changed since
    /// a prior run would leave a stale line in the old month — Bear never moves
    /// a note's creation date, so in practice the partition is stable.
    pub fn upsert_bear_notes(&self, notes: &[Note]) -> Result<()> {
        self.upsert_notes_by_month(NOTES_DIR, notes, |n| &n.created, |n| &n.id)
    }

    /// Upsert raw rows into `notes/bear/raw/YYYY-MM.jsonl`, partitioned by the
    /// month of the row's `_created` (immutable, so id-upsert is complete),
    /// deduped by `id` — same whole-affected-month-rewrite mechanics as the
    /// contract layer.
    fn upsert_bear_raw(&self, rows: &[Value]) -> Result<()> {
        self.upsert_raw_by_month(RAW_DIR, rows)
    }

    /// Generic "upsert these records into a month-partitioned snapshot,
    /// rewriting each affected month whole, deduped by key". `month_of` selects
    /// the RFC3339 timestamp whose month is the partition; `key_of` selects the
    /// dedupe key. Mirrors the per-affected-month atomic rewrite the notes
    /// contract specifies.
    fn upsert_notes_by_month(
        &self,
        dir: &str,
        fresh: &[Note],
        month_of: impl Fn(&Note) -> &str,
        key_of: impl Fn(&Note) -> &str,
    ) -> Result<()> {
        // Group the fresh notes by partition month.
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in fresh {
            let Some(key) = Partition::Month.key(month_of(n)) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(n);
        }
        let stream = self.stream(dir, Partition::Month);
        for (month, incoming) in by_month {
            // Read the existing month, drop any line whose id collides with an
            // incoming one, then append the incoming notes — a whole rewrite.
            let incoming_ids: HashSet<&str> = incoming.iter().map(|n| key_of(n)).collect();
            let mut merged: Vec<Note> = stream
                .read::<Note>(&month)?
                .into_iter()
                .filter(|existing| !incoming_ids.contains(key_of(existing)))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{dir}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Raw-layer twin of [`Self::upsert_notes_by_month`] over untyped JSON
    /// rows keyed by `id`, partitioned by `_created` (immutable).
    fn upsert_raw_by_month(&self, dir: &str, fresh: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in fresh {
            let Some(key) = Partition::Month.key(month_of(v)) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(v);
        }
        let stream = self.stream(dir, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|v| id_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|existing| !incoming_ids.contains(id_of(existing)))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{dir}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_bear_sync(&self) -> Option<BearSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_bear_sync(&self, state: &BearSyncState) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-bear-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Core Data seconds for a fixed local datetime.
    fn z_date(y: i32, m: u32, d: u32, h: u32) -> f64 {
        let t = Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) as f64
    }

    /// Build a synthetic Bear DB reproducing the REAL Core Data shape: ZSFNOTE
    /// + ZSFNOTETAG + a numbered `Z_<N>TAGS` join (we use `Z_5TAGS` with
    /// `Z_5NOTES`/`Z_13TAGS`, a real in-the-wild numbering, to exercise dynamic
    /// discovery). `pk_offset` lets two stores share a container without id
    /// clashes. Returns the path.
    fn fake_bear_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("trove-bear-db-{}-{name}.sqlite", std::process::id()));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZSFNOTE (
                Z_PK INTEGER PRIMARY KEY,
                ZUNIQUEIDENTIFIER TEXT,
                ZTITLE TEXT,
                ZTEXT TEXT,
                ZCREATIONDATE REAL,
                ZMODIFICATIONDATE REAL,
                ZTRASHED INTEGER,
                ZARCHIVED INTEGER,
                ZPINNED INTEGER,
                ZENCRYPTED INTEGER
            );
            CREATE TABLE ZSFNOTETAG (
                Z_PK INTEGER PRIMARY KEY,
                ZTITLE TEXT
            );
            CREATE TABLE Z_5TAGS (
                Z_5NOTES INTEGER,
                Z_13TAGS INTEGER
            );",
        )
        .unwrap();
        drop(conn);
        path
    }

    fn conn(path: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(path).unwrap()
    }

    /// Insert a note row. `flags` = (trashed, archived, pinned, encrypted).
    #[allow(clippy::too_many_arguments)]
    fn insert_note(
        c: &rusqlite::Connection,
        pk: i64,
        uid: &str,
        title: &str,
        text: &str,
        created: f64,
        modified: f64,
        flags: (i64, i64, i64, i64),
    ) {
        c.execute(
            "INSERT INTO ZSFNOTE
             (Z_PK, ZUNIQUEIDENTIFIER, ZTITLE, ZTEXT, ZCREATIONDATE, ZMODIFICATIONDATE,
              ZTRASHED, ZARCHIVED, ZPINNED, ZENCRYPTED)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![pk, uid, title, text, created, modified, flags.0, flags.1, flags.2, flags.3],
        )
        .unwrap();
    }

    fn insert_tag(c: &rusqlite::Connection, pk: i64, title: &str) {
        c.execute("INSERT INTO ZSFNOTETAG (Z_PK, ZTITLE) VALUES (?1, ?2)", rusqlite::params![pk, title]).unwrap();
    }

    fn link_tag(c: &rusqlite::Connection, note_pk: i64, tag_pk: i64) {
        c.execute("INSERT INTO Z_5TAGS (Z_5NOTES, Z_13TAGS) VALUES (?1, ?2)", rusqlite::params![note_pk, tag_pk])
            .unwrap();
    }

    #[test]
    fn core_data_epoch_conversion() {
        // 2026-06-10T00:00:00Z in Core Data seconds.
        let z = 1_781_049_600.0 - APPLE_EPOCH_OFFSET_S as f64;
        assert_eq!(core_data_to_local(z).unwrap().timestamp(), 1_781_049_600);
        let zf = z + 0.5;
        assert_eq!(core_data_to_local(zf).unwrap().timestamp(), 1_781_049_600);
        assert!(core_data_to_local(f64::NAN).is_none());
    }

    #[test]
    fn imports_tagged_archived_trashed_and_encrypted_notes() {
        let v = temp_vault("import");
        let db = fake_bear_db("import");
        let c = conn(&db);

        insert_tag(&c, 1, "garden");
        insert_tag(&c, 2, "spring");

        // A tagged, pinned note (created March, mod April).
        insert_note(
            &c, 10, "UID-GARDEN", "Garden planting plan",
            "# Garden planting plan\n\n- Tomatoes in the south bed\n- #garden #spring",
            z_date(2026, 3, 14, 9), z_date(2026, 4, 2, 18), (0, 0, 1, 0),
        );
        link_tag(&c, 10, 1);
        link_tag(&c, 10, 2);
        // An archived note.
        insert_note(&c, 11, "UID-ARCH", "Old plan", "archived body",
            z_date(2026, 5, 1, 8), z_date(2026, 5, 1, 8), (0, 1, 0, 0));
        // A trashed note.
        insert_note(&c, 12, "UID-TRASH", "Discarded", "trashed body",
            z_date(2026, 5, 2, 8), z_date(2026, 5, 2, 8), (1, 0, 0, 0));
        // An encrypted note: body must NOT be written; metadata + extra.encrypted.
        insert_note(&c, 13, "UID-ENC", "Locked thoughts", "OPAQUE-CIPHERTEXT-BLOB",
            z_date(2026, 5, 3, 8), z_date(2026, 5, 3, 8), (0, 0, 0, 1));
        drop(c);

        let (n, max) = import_bear_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 4);
        assert!(max > 0.0);

        // March contract file: the garden note, partitioned by CREATED month.
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar.len(), 1);
        let g = &mar[0];
        assert_eq!(g.source, "bear");
        assert_eq!(g.id, "UID-GARDEN");
        assert_eq!(g.title, "Garden planting plan");
        assert!(g.body.contains("Tomatoes"));
        assert_eq!(g.tags, vec!["garden", "spring"], "multi-tag join, tag-title order");
        assert_eq!(g.pinned, Some(true));
        assert_eq!(g.archived, None, "false flag omitted");
        assert_eq!(g.trashed, None);
        assert!(g.created.starts_with("2026-03-14"));
        assert!(g.modified.starts_with("2026-04-02"), "modified preserved");
        assert_eq!(g.folder, "", "Bear has no folder model");
        // Contract `extra` must NOT carry the unstable Core Data rowid.
        assert!(g.extra.get("z_pk").is_none(), "z_pk dropped from contract layer");
        assert!(g.extra.is_empty(), "non-encrypted note has empty contract extra");

        // May contract file: archived + trashed + encrypted (all created in May).
        let may = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may.len(), 3);
        let arch = may.iter().find(|n| n.id == "UID-ARCH").unwrap();
        assert_eq!(arch.archived, Some(true));
        let trash = may.iter().find(|n| n.id == "UID-TRASH").unwrap();
        assert_eq!(trash.trashed, Some(true));
        let enc = may.iter().find(|n| n.id == "UID-ENC").unwrap();
        assert_eq!(enc.body, "", "encrypted body is NEVER written as plaintext");
        assert_eq!(enc.title, "Locked thoughts", "plaintext title still emitted");
        assert_eq!(enc.extra.get("encrypted"), Some(&Value::Bool(true)));

        // Raw layer: full fidelity, partitioned by CREATED month (immutable).
        // Garden was created in March (modified April) → its raw row lives in
        // 2026-03, NOT 2026-04. ZTEXT present (not encrypted); flags verbatim;
        // the real modification date is preserved as a column. No row leaks into
        // the modification month.
        assert!(
            v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-04").unwrap().is_empty(),
            "raw is partitioned by created, not modified — nothing in April"
        );
        let raw_mar = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-03").unwrap();
        assert_eq!(raw_mar.len(), 1);
        assert_eq!(raw_mar[0]["id"], "UID-GARDEN");
        assert!(raw_mar[0]["ztext"].as_str().unwrap().contains("Tomatoes"));
        assert_eq!(raw_mar[0]["zpinned"], 1, "raw keeps the integer flag verbatim");
        assert_eq!(raw_mar[0]["z_pk"], 10, "raw keeps full fidelity incl. z_pk");
        assert!(raw_mar[0]["_created"].as_str().unwrap().starts_with("2026-03"));
        // Encrypted note's raw row withholds ztext but records encrypted:true.
        let raw_may = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-05").unwrap();
        let raw_enc = raw_may.iter().find(|r| r["id"] == "UID-ENC").unwrap();
        assert_eq!(raw_enc["encrypted"], Value::Bool(true));
        assert!(raw_enc.get("ztext").is_none(), "ciphertext withheld from raw too");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn incremental_watermark_and_dedupe_upsert_by_id() {
        let v = temp_vault("incremental");
        let db = fake_bear_db("incremental");
        let c = conn(&db);
        insert_note(&c, 1, "UID-A", "A", "first body",
            z_date(2026, 6, 1, 9), z_date(2026, 6, 1, 9), (0, 0, 0, 0));
        drop(c);

        let (n1, max1) = import_bear_db(&v, &db, 0.0).unwrap();
        assert_eq!(n1, 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june[0].body, "first body");

        // Re-run from the watermark with nothing newer → no work, one line.
        let (n2, max2) = import_bear_db(&v, &db, max1).unwrap();
        assert_eq!(n2, 0, "watermark skips unchanged rows");
        assert_eq!(max2, max1);

        // Edit the note (same id, later mod date) → upsert replaces the line.
        let c = conn(&db);
        c.execute(
            "UPDATE ZSFNOTE SET ZTEXT = 'edited body', ZMODIFICATIONDATE = ?1 WHERE Z_PK = 1",
            [z_date(2026, 6, 5, 12)],
        )
        .unwrap();
        drop(c);
        let (n3, _) = import_bear_db(&v, &db, max1).unwrap();
        assert_eq!(n3, 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june.len(), 1, "upsert by id: exactly one line, not two");
        assert_eq!(june[0].body, "edited body", "the line was replaced in place");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn cross_month_edit_leaves_exactly_one_row_in_the_created_month() {
        // Regression: a note created in month A then edited so its modification
        // time lands in month B must NOT leave a stale duplicate. Because both
        // layers partition by the immutable `created`, the note's contract row
        // AND raw row both stay in A's file and the edit upserts them in place —
        // no orphan in B carrying the old body. (A modified-month raw partition
        // would have stranded the old row in A while writing a new one in B.)
        let v = temp_vault("crossmonth");
        let db = fake_bear_db("crossmonth");
        let c = conn(&db);
        // Created in March, first modified in March.
        insert_note(&c, 1, "UID-X", "X", "march body",
            z_date(2026, 3, 10, 9), z_date(2026, 3, 10, 9), (0, 0, 0, 0));
        drop(c);
        let (_, max1) = import_bear_db(&v, &db, 0.0).unwrap();

        // Edit in June: same id+created, modification date now in June.
        let c = conn(&db);
        c.execute(
            "UPDATE ZSFNOTE SET ZTEXT = 'june body', ZMODIFICATIONDATE = ?1 WHERE Z_PK = 1",
            [z_date(2026, 6, 20, 14)],
        )
        .unwrap();
        drop(c);
        let (n, _) = import_bear_db(&v, &db, max1).unwrap();
        assert_eq!(n, 1);

        // Contract: exactly one row, in March (created month), with the new body.
        let mar_c = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar_c.len(), 1, "one contract row, in the created month");
        assert_eq!(mar_c[0].body, "june body");
        assert!(mar_c[0].modified.starts_with("2026-06-20"), "modified updated in place");
        assert!(
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap().is_empty(),
            "no contract orphan in the modification month"
        );

        // Raw: exactly ONE row across all months, in March, carrying the NEW body.
        let mar_r = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-03").unwrap();
        assert_eq!(mar_r.len(), 1, "exactly one raw row, in the created month");
        assert_eq!(mar_r[0]["id"], "UID-X");
        assert_eq!(mar_r[0]["ztext"], "june body", "raw row carries the fresh body, no stale dup");
        assert!(
            v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-06").unwrap().is_empty(),
            "no raw orphan in the modification month"
        );

        let _ = fs::remove_file(db);
    }

    #[test]
    fn null_modification_date_never_fabricates_a_2001_row() {
        // A row whose ZMODIFICATIONDATE is NULL must NEVER produce a fabricated
        // 2001-01-01 `modified` or a bogus watermark from coercing NULL→0.0.
        // Two facts together prove the fix:
        //   (a) SQLite's `ZMODIFICATIONDATE > cursor` excludes a NULL row (NULL
        //       compares as NULL, never true) — so it is simply not imported,
        //       and a sane `created`-only note is never displaced by a 2001 one.
        //   (b) the watermark only advances on a finite mod date, so a NULL row
        //       can't drag the cursor back to ~−978M (the 0.0 → 2001 artifact).
        // We seed one NULL-mod row beside one normal row and confirm only the
        // normal row lands, in its real month, and the watermark is its date.
        let v = temp_vault("nullmod");
        let db = fake_bear_db("nullmod");
        let c = conn(&db);
        c.execute(
            "INSERT INTO ZSFNOTE (Z_PK, ZUNIQUEIDENTIFIER, ZTITLE, ZTEXT, ZCREATIONDATE, ZMODIFICATIONDATE)
             VALUES (1, 'UID-NULLMOD', 'No mod date', 'body', ?1, NULL)",
            [z_date(2026, 4, 1, 9)],
        )
        .unwrap();
        insert_note(&c, 2, "UID-OK", "Has mod", "ok",
            z_date(2026, 6, 2, 9), z_date(2026, 6, 2, 9), (0, 0, 0, 0));
        drop(c);

        let (n, max) = import_bear_db(&v, &db, 0.0).unwrap();
        assert_eq!(n, 1, "NULL-mod row excluded by the incremental filter");
        // No fabricated 2001-01-* file anywhere.
        assert!(
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2001-01").unwrap().is_empty(),
            "no 2001 row fabricated from NULL→0.0"
        );
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june.len(), 1);
        assert_eq!(june[0].id, "UID-OK");
        assert!(june[0].modified.starts_with("2026-06-02"));
        // Watermark is the normal row's finite date, not ~−978M (the 0.0 bug).
        assert!(max > 0.0, "watermark advanced to a real date, not 0.0/2001");
        let _ = fs::remove_file(db);
    }

    #[test]
    fn schema_adaptive_select_tolerates_missing_columns() {
        // An older/leaner Bear schema: no ZPINNED / ZARCHIVED / ZENCRYPTED, and
        // no tag join table at all. The SELECT must still succeed and write a
        // row with those flags omitted and no tags.
        let v = temp_vault("lean");
        let path = std::env::temp_dir().join(format!("trove-bear-lean-{}.sqlite", std::process::id()));
        let _ = fs::remove_file(&path);
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE ZSFNOTE (
                Z_PK INTEGER PRIMARY KEY,
                ZUNIQUEIDENTIFIER TEXT,
                ZTITLE TEXT,
                ZTEXT TEXT,
                ZCREATIONDATE REAL,
                ZMODIFICATIONDATE REAL,
                ZTRASHED INTEGER
            );",
        )
        .unwrap();
        c.execute(
            "INSERT INTO ZSFNOTE (Z_PK, ZUNIQUEIDENTIFIER, ZTITLE, ZTEXT, ZCREATIONDATE, ZMODIFICATIONDATE, ZTRASHED)
             VALUES (1, 'UID-LEAN', 'Lean note', 'lean body', ?1, ?1, 0)",
            [z_date(2026, 6, 10, 9)],
        )
        .unwrap();
        drop(c);

        let (n, _) = import_bear_db(&v, &path, 0.0).unwrap();
        assert_eq!(n, 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june[0].id, "UID-LEAN");
        assert_eq!(june[0].pinned, None, "missing ZPINNED column omitted");
        assert_eq!(june[0].archived, None);
        assert!(june[0].tags.is_empty(), "no join table → no tags, no error");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn discovers_alternate_join_table_numbering() {
        // The same library on a different Bear version: the join table is
        // `Z_7TAGS(Z_7NOTES, Z_14TAGS)` instead of `Z_5TAGS`. Discovery must
        // find it by the `Z_%TAGS` + `…NOTES`/`…TAGS` pattern, not a hardcoded
        // number.
        let v = temp_vault("altjoin");
        let path = std::env::temp_dir().join(format!("trove-bear-alt-{}.sqlite", std::process::id()));
        let _ = fs::remove_file(&path);
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE ZSFNOTE (Z_PK INTEGER PRIMARY KEY, ZUNIQUEIDENTIFIER TEXT, ZTEXT TEXT, ZCREATIONDATE REAL, ZMODIFICATIONDATE REAL);
             CREATE TABLE ZSFNOTETAG (Z_PK INTEGER PRIMARY KEY, ZTITLE TEXT);
             CREATE TABLE Z_7TAGS (Z_7NOTES INTEGER, Z_14TAGS INTEGER);",
        )
        .unwrap();
        c.execute("INSERT INTO ZSFNOTETAG (Z_PK, ZTITLE) VALUES (1, 'work')", []).unwrap();
        c.execute(
            "INSERT INTO ZSFNOTE (Z_PK, ZUNIQUEIDENTIFIER, ZTEXT, ZCREATIONDATE, ZMODIFICATIONDATE) VALUES (1, 'UID-ALT', 'body', ?1, ?1)",
            [z_date(2026, 6, 9, 9)],
        )
        .unwrap();
        c.execute("INSERT INTO Z_7TAGS (Z_7NOTES, Z_14TAGS) VALUES (1, 1)", []).unwrap();

        let join = discover_tag_join(&c).unwrap().expect("Z_7TAGS discovered");
        assert_eq!(join.table, "Z_7TAGS");
        assert_eq!(join.notes_col, "Z_7NOTES");
        assert_eq!(join.tags_col, "Z_14TAGS");
        drop(c);

        let (n, _) = import_bear_db(&v, &path, 0.0).unwrap();
        assert_eq!(n, 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june[0].tags, vec!["work"], "tags resolved via the alternate join");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn two_bears_share_one_container() {
        // Bear 1 and Bear 2 read the same DB path. Simulate a second pass over
        // a DB that grew (a new note added), confirming the incremental scan
        // picks up only the addition and the whole library is consistent.
        let v = temp_vault("twobears");
        let db = fake_bear_db("twobears");
        let c = conn(&db);
        insert_note(&c, 1, "UID-1", "One", "one",
            z_date(2026, 6, 1, 9), z_date(2026, 6, 1, 9), (0, 0, 0, 0));
        drop(c);
        let (_, max1) = import_bear_db(&v, &db, 0.0).unwrap();

        let c = conn(&db);
        insert_note(&c, 2, "UID-2", "Two", "two",
            z_date(2026, 6, 2, 9), z_date(2026, 6, 2, 9), (0, 0, 0, 0));
        drop(c);
        let (n, _) = import_bear_db(&v, &db, max1).unwrap();
        assert_eq!(n, 1, "only the new note re-read");
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june.len(), 2);
        let _ = fs::remove_file(db);
    }

    #[test]
    fn watermark_persists_and_round_trips() {
        let v = temp_vault("watermark");
        assert!(v.read_bear_sync().is_none());
        let state = BearSyncState { updated: "2026-06-14T00:00:00-07:00".into(), cursor: 555.5 };
        v.write_bear_sync(&state).unwrap();
        let got = v.read_bear_sync().unwrap();
        assert_eq!(got.cursor, 555.5);
        assert_eq!(got.updated, "2026-06-14T00:00:00-07:00");
    }

    #[test]
    fn fda_unreadable_is_graceful_no_op() {
        // With TROVE_HOME pointed at an empty temp tree, the DB doesn't exist →
        // permission_ok is false → collect is a no-op with available:false.
        let _g = env_guard();
        let fake_home = std::env::temp_dir().join(format!("trove-bear-nohome-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fake_home);
        fs::create_dir_all(&fake_home).unwrap();
        std::env::set_var("TROVE_HOME", &fake_home);

        assert!(!bear_permission_ok(), "no DB under empty home");
        let v = temp_vault("noop");
        let stats = v.collect_bear().unwrap();
        assert!(!stats.available);
        assert_eq!(stats.new_notes, 0);

        std::env::remove_var("TROVE_HOME");
    }

    #[test]
    fn serde_back_compat_old_lines_still_deserialize() {
        // A Note line written by a hypothetical older/leaner writer (only the
        // required fields) must still deserialize — additive evolution.
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-06.jsonl")),
            "{\"source\":\"bear\",\"id\":\"OLD-1\"}\n{\"source\":\"bear\",\"id\":\"OLD-2\",\"body\":\"b\",\"created\":\"2026-06-01T00:00:00-07:00\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "OLD-1");
        assert_eq!(rows[1].body, "b");
    }

    /// Serialize the env-mutating test so parallel tests don't see TROVE_HOME.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
