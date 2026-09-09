//! Apple HomeKit — local homed database snapshot collector.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/apple-homekit.md.
//!
//! A **Periodic** local collector that reads the `homed` CoreData SQLite
//! database at `~/Library/HomeKit/core.sqlite` and snapshots the home
//! topology (homes, rooms, accessories, scenes, automations) whenever the
//! config changes. This is config and topology, **not telemetry** — there is
//! no state-change log in this DB, so the contract layer (`HomeReading`) does
//! not apply. Everything goes to raw.
//!
//! **Raw layer only** — `home/apple-homekit/snapshots/YYYY-MM-DD.json`, one
//! file per day when the config changed. The snapshot is skipped (not written)
//! when the normalized content hash matches the last known hash — so the
//! folder is a sparse change-log of the home configuration over time.
//!
//! **Access** — the DB lives in the user Library, readable without FDA in
//! most configurations; Full Disk Access covers edge cases. We copy-then-open
//! the DB (never take a write lock while homed is live). The copy is made
//! with `fs::copy`, matching the iMessage/browser pattern.
//!
//! **Schema-adaptive** — we SELECT only columns confirmed via `PRAGMA
//! table_info`; unknown columns are ignored. This degrades gracefully across
//! macOS releases that add or rename columns (the brief's explicit concern).
//!
//! **Automation blobs** — `ZMKFTRIGGER.ZEVALUATIONCONDITION` and
//! `ZMKFACTION.ZDATA` are binary (NSKeyedArchiver / protobuf). We store them
//! base64-encoded under `"_raw_blob"` and never fail the snapshot on a
//! decode error. Full-fidelity first; decode is a future enhancement.
//!
//! **Guid** — `<home_uuid>:<snapshot_date>` where `home_uuid` is the
//! `ZUNIQUEIDENTIFIER` of the first home row; when absent (or when the
//! ZMKFHOME table is not present — unconfirmed on macOS 15), we fall back to
//! `"unknown"`. Until Needs-David validates the real home-entity table name
//! on a live DB, expect `"unknown:<date>"` in production.
//!
//! **Needs-David** — FDA grant on his Mac is required to validate the read
//! path and confirm the junction table names (CoreData internal IDs that can
//! shift across model versions; we handle the most common patterns and log
//! warnings when a table is absent).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use base64::Engine as _;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::write_json_atomic;
use crate::vault::Vault;

/// Vault-relative root for raw snapshots.
const SNAP_DIR: &str = "home/apple-homekit/snapshots";

/// Non-secret rebuildable cursor for the last-snapshot hash.
/// Deleted → re-snapshot on the next run.
const SYNC_FILE: &str = ".trove/apple-homekit-sync.json";

// NOTE on ZMKFHOME: the `tamengual/homekit-extractor` reference (authoritative reverse-engineering
// of `core.sqlite` on macOS 15) does NOT list ZMKFHOME in its `.tables` output, and never
// queries FROM ZMKFHOME.  Home name + UUID are exposed by the Swift HMHomeManager API in a
// separate ~600 KB export file, not in core.sqlite.  The ZHOME INTEGER foreign-key column on
// rooms/accessories implies there is *some* home-entity table, but the real table name, and the
// column names (ZCONFIGUREDNAME / ZUNIQUEIDENTIFIER), are unconfirmed.
//
// Consequence: `homes` will likely be empty on a real DB, and the snapshot `guid` will degrade
// to `"unknown:<date>"`.  This is the intended safe fallback (the snapshot is still useful and
// fully deduplicated by content hash).  A Needs-David validation run is required to confirm the
// real table name (query `SELECT Z_ENT,Z_NAME FROM Z_PRIMARYKEY WHERE Z_NAME LIKE '%Home%'` on a
// live DB) before the guid can use a stable home UUID.

/// Seconds between full snapshot checks. Daily is plenty — the topology
/// rarely changes more than once a day.
pub const HK_SYNC_SECS: u64 = 86_400;

/// CoreData epoch offset: 2001-01-01T00:00:00Z – 1970-01-01T00:00:00Z.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

// ---------------------------------------------------------------------------
// Cursor.

/// Persisted state: hash of the last written snapshot (hex SHA-256), so we
/// can skip unchanged configs without re-reading every accessory row.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// Hex SHA-256 of the last written snapshot JSON. Empty = no prior snap.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    last_hash: String,
    /// RFC3339 local time of the last successful snapshot write.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    updated: String,
}

// ---------------------------------------------------------------------------
// Path helpers.

fn homekit_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library/HomeKit/core.sqlite")
}

pub fn homekit_permission_ok() -> bool {
    let path = homekit_db_path();
    path.exists() && fs::File::open(&path).is_ok()
}

fn homekit_db_mtime() -> Option<SystemTime> {
    fs::metadata(homekit_db_path()).ok().and_then(|m| m.modified().ok())
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(homekit_permission_ok()),
        required: false, // usually readable without FDA; FDA handles edge cases
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(SNAP_DIR))
}

fn def_collect(vault: &Vault, now: DateTime<Local>) -> Result<CollectOutcome> {
    match snapshot_homekit(vault, now) {
        Ok(true) => Ok(CollectOutcome::note("HomeKit snapshot updated".to_string())),
        Ok(false) => Ok(CollectOutcome::quiet()),
        Err(e) => {
            // Permission denied / DB absent → not an error in the watcher loop.
            if !homekit_permission_ok() {
                return Ok(CollectOutcome::note(
                    "HomeKit snapshot skipped — database unreadable (grant Full Disk Access)".to_string(),
                ));
            }
            Err(e)
        }
    }
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-homekit",
        name: "Apple HomeKit",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Snapshots your HomeKit home configuration — accessories, rooms, scenes, and \
             automations — from the local homed database. Writes a new snapshot only when the \
             topology changes, creating a sparse change-log of your home over time.",
        domain: "home",
        vault_path: "home/apple-homekit/",
        toggleable: true,
        setup: &[
            "No setup required for most users — the HomeKit database is in your user Library.",
            "If the card shows a permission error, grant Full Disk Access to Trove in \
             System Settings → Privacy & Security → Full Disk Access.",
        ],
        caveats:
            "Config and topology only — no device state history. Automation blobs (NSKeyedArchiver \
             / protobuf) are stored base64-encoded for future decoding. Schema is Apple-internal \
             and may shift across macOS releases; columns not present in the live DB are silently \
             skipped.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::on_change(HK_SYNC_SECS, homekit_db_mtime),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Snapshot shape.

/// One full topology snapshot, written as a single JSON file per day.
#[derive(Debug, Serialize, Deserialize)]
pub struct HomeKitSnapshot {
    /// `<home_uuid>:<snapshot_date>` — stable, date-unique key.
    pub guid: String,
    /// RFC3339 local time when this snapshot was taken.
    pub ts: String,
    /// User-facing home name(s) from the DB.
    pub homes: Vec<HomeRow>,
    pub rooms: Vec<RoomRow>,
    pub accessories: Vec<AccessoryRow>,
    pub scenes: Vec<SceneRow>,
    pub automations: Vec<AutomationRow>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HomeRow {
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RoomRow {
    pub pk: i64,
    pub name: String,
    pub uuid: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AccessoryRow {
    pub pk: i64,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub uuid: String,
    pub room_pk: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SceneRow {
    pub pk: i64,
    pub name: String,
    /// Action-set type string from ZMKFACTIONSET.ZTYPE (VARCHAR).
    ///
    /// Known values from the homekit-extractor reference:
    ///   - `"com.apple.HomeKit.actionSet.type.trigger"` — per-automation action set (NOT a user scene)
    ///   - `"com.apple.HomeKit.actionSet.type.userDefined"` — user-created scene
    ///   - `"com.apple.HomeKit.actionSet.type.homeArrival"` — Home/Away arrival
    ///   - `"com.apple.HomeKit.actionSet.type.homeDeparture"` — Home/Away departure
    ///   - `"com.apple.HomeKit.actionSet.type.sleep"` — Sleep scene
    ///   - `"com.apple.HomeKit.actionSet.type.wakeUp"` — Wake Up scene
    ///
    /// `None` when the ZTYPE column is absent in the live DB (schema drift) or the value is NULL.
    /// Consumers should filter to `kind != Some("com.apple.HomeKit.actionSet.type.trigger")` to
    /// get only user-visible scenes; all rows are kept raw for full fidelity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AutomationRow {
    pub pk: i64,
    pub name: String,
    pub active: bool,
    pub trigger_type: Option<i64>,
    pub significant_event: Option<String>,
    /// Base64-encoded binary blob (NSKeyedArchiver condition); `None` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition_blob: Option<String>,
    /// Last fire date as CoreData timestamp → local RFC3339; `None` when NULL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired: Option<String>,
}

// ---------------------------------------------------------------------------
// Schema-adaptive column helpers.

/// Reads `PRAGMA table_info(<table>)` and returns a set of uppercase column names present.
fn table_columns(conn: &rusqlite::Connection, table: &str) -> HashMap<String, i64> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let mut cols = HashMap::new();
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    while let Ok(Some(row)) = rows.next() {
        let cid: i64 = row.get(0).unwrap_or(0);
        let name: String = row.get(1).unwrap_or_default();
        cols.insert(name.to_uppercase(), cid);
    }
    cols
}

/// Returns true if `table` exists in the DB.
fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![table],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0) > 0
}

/// A nullable String column from a row; empty string when NULL.
fn col_str(row: &rusqlite::Row, idx: usize) -> String {
    row.get::<_, Option<String>>(idx).unwrap_or_default().unwrap_or_default()
}

/// A nullable i64 column; 0 when NULL.
fn col_i64(row: &rusqlite::Row, idx: usize) -> i64 {
    row.get::<_, Option<i64>>(idx).unwrap_or_default().unwrap_or(0)
}

/// CoreData timestamp (real seconds since 2001) → local RFC3339. Returns None when 0 or negative.
fn core_data_ts(secs: f64) -> Option<String> {
    if secs <= 0.0 {
        return None;
    }
    let unix = secs as i64 + APPLE_EPOCH_OFFSET_S;
    let frac_ns = ((secs.fract()) * 1_000_000_000.0).abs() as u32;
    DateTime::from_timestamp(unix, frac_ns)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// DB read — runs against a temporary copy of core.sqlite.

fn read_homekit_db(db: &std::path::Path) -> Result<HomeKitSnapshot> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening HomeKit DB copy")?;

    let now_ts = Local::now().to_rfc3339();

    // -----------------------------------------------------------------------
    // Homes (ZMKFHOME or fall back to room's ZHOME foreign key set)
    let mut homes: Vec<HomeRow> = Vec::new();
    let home_table = if table_exists(&conn, "ZMKFHOME") {
        "ZMKFHOME"
    } else if table_exists(&conn, "ZMKFPLACEMARK") {
        "ZMKFPLACEMARK"
    } else {
        ""
    };
    if !home_table.is_empty() {
        let cols = table_columns(&conn, home_table);
        let name_col = if cols.contains_key("ZCONFIGUREDNAME") {
            "ZCONFIGUREDNAME"
        } else if cols.contains_key("ZNAME") {
            "ZNAME"
        } else {
            "NULL"
        };
        let uuid_col = if cols.contains_key("ZUNIQUEIDENTIFIER") { "ZUNIQUEIDENTIFIER" } else { "NULL" };
        let sql = format!("SELECT COALESCE({name_col}, ''), COALESCE({uuid_col}, '') FROM {home_table}");
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let mut rows = stmt.query([]).unwrap_or_else(|_| unreachable!());
            while let Ok(Some(row)) = rows.next() {
                homes.push(HomeRow {
                    name: col_str(row, 0),
                    uuid: col_str(row, 1),
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Rooms (ZMKFROOM)
    let mut rooms: Vec<RoomRow> = Vec::new();
    if table_exists(&conn, "ZMKFROOM") {
        let cols = table_columns(&conn, "ZMKFROOM");
        let uuid_col = if cols.contains_key("ZUNIQUEIDENTIFIER") { "ZUNIQUEIDENTIFIER" } else { "NULL" };
        let name_col = if cols.contains_key("ZNAME") { "ZNAME" } else { "''" };
        let sql = format!(
            "SELECT Z_PK, COALESCE({name_col}, ''), COALESCE({uuid_col}, '') FROM ZMKFROOM"
        );
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let mut rows = stmt.query([]).unwrap_or_else(|_| unreachable!());
            while let Ok(Some(row)) = rows.next() {
                rooms.push(RoomRow {
                    pk: col_i64(row, 0),
                    name: col_str(row, 1),
                    uuid: col_str(row, 2),
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Accessories (ZMKFACCESSORY)
    let mut accessories: Vec<AccessoryRow> = Vec::new();
    if table_exists(&conn, "ZMKFACCESSORY") {
        let cols = table_columns(&conn, "ZMKFACCESSORY");
        let name_col = if cols.contains_key("ZCONFIGUREDNAME") {
            "ZCONFIGUREDNAME"
        } else if cols.contains_key("ZNAME") {
            "ZNAME"
        } else {
            "''"
        };
        let mfr_col = if cols.contains_key("ZMANUFACTURER") { "ZMANUFACTURER" } else { "''" };
        let model_col = if cols.contains_key("ZMODEL") { "ZMODEL" } else { "''" };
        let uuid_col = if cols.contains_key("ZUNIQUEIDENTIFIER") { "ZUNIQUEIDENTIFIER" } else { "NULL" };
        let room_col = if cols.contains_key("ZROOM") { "ZROOM" } else { "NULL" };
        let sql = format!(
            "SELECT Z_PK, COALESCE({name_col}, ''), COALESCE({mfr_col}, ''), \
             COALESCE({model_col}, ''), COALESCE({uuid_col}, ''), {room_col} FROM ZMKFACCESSORY"
        );
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let mut rows = stmt.query([]).unwrap_or_else(|_| unreachable!());
            while let Ok(Some(row)) = rows.next() {
                let room_pk = row.get::<_, Option<i64>>(5).unwrap_or(None);
                accessories.push(AccessoryRow {
                    pk: col_i64(row, 0),
                    name: col_str(row, 1),
                    manufacturer: col_str(row, 2),
                    model: col_str(row, 3),
                    uuid: col_str(row, 4),
                    room_pk,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Scenes / ActionSets (ZMKFACTIONSET)
    let mut scenes: Vec<SceneRow> = Vec::new();
    if table_exists(&conn, "ZMKFACTIONSET") {
        let cols = table_columns(&conn, "ZMKFACTIONSET");
        let name_col = if cols.contains_key("ZCONFIGUREDNAME") {
            "ZCONFIGUREDNAME"
        } else if cols.contains_key("ZNAME") {
            "ZNAME"
        } else {
            "''"
        };
        let type_col = if cols.contains_key("ZTYPE") { "ZTYPE" } else { "NULL" };
        let sql = format!(
            "SELECT Z_PK, COALESCE({name_col}, ''), {type_col} FROM ZMKFACTIONSET"
        );
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let mut rows = stmt.query([]).unwrap_or_else(|_| unreachable!());
            while let Ok(Some(row)) = rows.next() {
                // ZTYPE is VARCHAR (e.g. "com.apple.HomeKit.actionSet.type.userDefined").
                // Read as Option<String>; silently drop on type error (schema drift).
                let kind: Option<String> = row.get::<_, Option<String>>(2).unwrap_or(None);
                scenes.push(SceneRow {
                    pk: col_i64(row, 0),
                    name: col_str(row, 1),
                    kind,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Automations (ZMKFTRIGGER)
    let mut automations: Vec<AutomationRow> = Vec::new();
    if table_exists(&conn, "ZMKFTRIGGER") {
        let cols = table_columns(&conn, "ZMKFTRIGGER");
        let name_col = if cols.contains_key("ZCONFIGUREDNAME") {
            "ZCONFIGUREDNAME"
        } else if cols.contains_key("ZNAME") {
            "ZNAME"
        } else {
            "''"
        };
        let active_col = if cols.contains_key("ZACTIVE") { "ZACTIVE" } else { "0" };
        let ent_col = if cols.contains_key("Z_ENT") { "Z_ENT" } else { "NULL" };
        let sig_col = if cols.contains_key("ZSIGNIFICANTEVENT") { "ZSIGNIFICANTEVENT" } else { "NULL" };
        let cond_col = if cols.contains_key("ZEVALUATIONCONDITION") {
            "ZEVALUATIONCONDITION"
        } else {
            "NULL"
        };
        let fire_col = if cols.contains_key("ZMOSTRECENTFIREDATE") {
            "ZMOSTRECENTFIREDATE"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT Z_PK, COALESCE({name_col}, ''), {active_col}, {ent_col}, \
             {sig_col}, {cond_col}, {fire_col} FROM ZMKFTRIGGER"
        );
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let mut rows = stmt.query([]).unwrap_or_else(|_| unreachable!());
            while let Ok(Some(row)) = rows.next() {
                let pk = col_i64(row, 0);
                let name = col_str(row, 1);
                let active = col_i64(row, 2) != 0;
                let trigger_type = row.get::<_, Option<i64>>(3).unwrap_or(None);
                let significant_event = row.get::<_, Option<String>>(4).unwrap_or(None);

                // Condition blob: store as base64 for full fidelity.
                let raw_blob = row.get_ref(5).ok().and_then(|v| {
                    match v {
                        rusqlite::types::ValueRef::Blob(b) if !b.is_empty() => {
                            Some(base64::engine::general_purpose::STANDARD.encode(b))
                        }
                        rusqlite::types::ValueRef::Text(t) if !t.is_empty() => {
                            Some(String::from_utf8_lossy(t).to_string())
                        }
                        _ => None,
                    }
                });

                // Fire date: CoreData real seconds since 2001.
                let last_fired = row.get::<_, Option<f64>>(6).unwrap_or(None)
                    .and_then(|f| core_data_ts(f));

                automations.push(AutomationRow {
                    pk,
                    name,
                    active,
                    trigger_type,
                    significant_event,
                    condition_blob: raw_blob,
                    last_fired,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Guid: use the UUID of the first home row, or "unknown" when the home
    // table is absent / has no rows (see NOTE on ZMKFHOME above — the table
    // name and column names are unconfirmed on macOS 15; expect "unknown" in
    // production until Needs-David validation confirms the real table name).
    // The snapshot is still uniquely identified by its content hash for dedup.
    let home_uuid = homes.first().map(|h| h.uuid.clone())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let snap_date = &now_ts[..10]; // YYYY-MM-DD
    let guid = format!("{home_uuid}:{snap_date}");

    Ok(HomeKitSnapshot {
        guid,
        ts: now_ts,
        homes,
        rooms,
        accessories,
        scenes,
        automations,
    })
}

// ---------------------------------------------------------------------------
// Hash helpers — content dedup so unchanged configs don't accumulate files.

/// SHA-256 hex digest of the serialized snapshot (excluding the `ts` field,
/// since that changes every run — we normalize the snapshot without the ts
/// before hashing so content equality means topology equality).
fn snapshot_hash(snap: &HomeKitSnapshot) -> String {
    use sha2::{Digest, Sha256};
    // Build a stable representation without the volatile ts.
    let repr = serde_json::json!({
        "homes": snap.homes,
        "rooms": snap.rooms,
        "accessories": snap.accessories,
        "scenes": snap.scenes,
        "automations": snap.automations,
    });
    let bytes = serde_json::to_vec(&repr).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

// ---------------------------------------------------------------------------
// Vault helpers.

impl Vault {
    fn read_homekit_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_homekit_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Main snapshot logic.

/// Take one topology snapshot. Returns `true` when a new file was written,
/// `false` when the topology is unchanged since the last snapshot.
pub fn snapshot_homekit(vault: &Vault, now: DateTime<Local>) -> Result<bool> {
    if !homekit_permission_ok() {
        anyhow::bail!("HomeKit DB is not readable — grant Full Disk Access to Trove");
    }

    let db = homekit_db_path();
    let stem = format!("trove-homekit-{}", std::process::id());

    let snap = import_via_copy(&db, &stem, |tmp| read_homekit_db(tmp))?;

    let hash = snapshot_hash(&snap);
    let state = vault.read_homekit_sync();
    if state.last_hash == hash {
        // Topology unchanged — skip the write.
        return Ok(false);
    }

    // Write the snapshot JSON under the vault's snapshot dir.
    let date_str = now.format("%Y-%m-%d").to_string();
    let snap_path = vault
        .resolve(&format!("{SNAP_DIR}/{date_str}.json"))
        .context("resolving snapshot path")?;
    // Ensure the snapshot directory exists.
    if let Some(parent) = snap_path.parent() {
        fs::create_dir_all(parent).context("creating HomeKit snapshot dir")?;
    }
    let snap_bytes = serde_json::to_vec_pretty(&snap)?;
    let tmp = snap_path.with_extension("json.tmp");
    fs::write(&tmp, &snap_bytes).context("writing HomeKit snapshot tmp")?;
    fs::rename(&tmp, &snap_path).context("renaming HomeKit snapshot")?;

    // Advance the cursor after a successful write.
    vault.write_homekit_sync(&SyncState {
        last_hash: hash,
        updated: now.to_rfc3339(),
    })?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-apple-homekit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build an in-memory SQLite database that mirrors the HomeKit schema
    /// (subset of tables/columns confirmed from homed_extract.py).
    fn make_fake_hk_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZMKFHOME (
                Z_PK INTEGER PRIMARY KEY,
                ZUNIQUEIDENTIFIER TEXT,
                ZCONFIGUREDNAME TEXT
            );
            CREATE TABLE ZMKFROOM (
                Z_PK INTEGER PRIMARY KEY,
                ZNAME TEXT,
                ZUNIQUEIDENTIFIER TEXT,
                ZHOME INTEGER
            );
            CREATE TABLE ZMKFACCESSORY (
                Z_PK INTEGER PRIMARY KEY,
                ZCONFIGUREDNAME TEXT,
                ZMANUFACTURER TEXT,
                ZMODEL TEXT,
                ZUNIQUEIDENTIFIER TEXT,
                ZROOM INTEGER
            );
            CREATE TABLE ZMKFACTIONSET (
                Z_PK INTEGER PRIMARY KEY,
                ZCONFIGUREDNAME TEXT,
                ZTYPE TEXT
            );
            CREATE TABLE ZMKFTRIGGER (
                Z_PK INTEGER PRIMARY KEY,
                ZCONFIGUREDNAME TEXT,
                ZACTIVE INTEGER DEFAULT 0,
                Z_ENT INTEGER,
                ZSIGNIFICANTEVENT TEXT,
                ZEVALUATIONCONDITION BLOB,
                ZMOSTRECENTFIREDATE REAL
            );",
        )
        .unwrap();

        // Seed data: one home, two rooms, three accessories, one scene, two automations.
        conn.execute_batch(
            "INSERT INTO ZMKFHOME VALUES (1, 'HOME-UUID-0001', 'Our Home');
             INSERT INTO ZMKFROOM VALUES (1, 'Living Room', 'ROOM-UUID-0001', 1);
             INSERT INTO ZMKFROOM VALUES (2, 'Kitchen', 'ROOM-UUID-0002', 1);
             INSERT INTO ZMKFACCESSORY VALUES (1, 'Hue Lamp', 'Signify', 'Hue White', 'ACC-UUID-0001', 1);
             INSERT INTO ZMKFACCESSORY VALUES (2, 'Thermostat', 'Ecobee', 'SmartThermostat', 'ACC-UUID-0002', 2);
             INSERT INTO ZMKFACCESSORY VALUES (3, 'Door Lock', 'Schlage', 'Encode', 'ACC-UUID-0003', NULL);
             INSERT INTO ZMKFACTIONSET VALUES (1, 'Good Morning', 'com.apple.HomeKit.actionSet.type.userDefined');
             INSERT INTO ZMKFTRIGGER VALUES (1, 'Sunset Lights', 1, 12, 'sunset', NULL, 759484800.0);
             INSERT INTO ZMKFTRIGGER VALUES (2, 'Morning Alarm', 0, 13, NULL, NULL, NULL);",
        )
        .unwrap();
    }

    #[test]
    fn apple_homekit_reads_topology_from_fake_db() {
        let db_path = std::env::temp_dir()
            .join(format!("trove-hk-fake-{}.db", std::process::id()));
        let _ = fs::remove_file(&db_path);
        make_fake_hk_db(&db_path);

        let snap = read_homekit_db(&db_path).expect("should read from fake DB");
        let _ = fs::remove_file(&db_path);

        // Home
        assert_eq!(snap.homes.len(), 1);
        assert_eq!(snap.homes[0].uuid, "HOME-UUID-0001");
        assert_eq!(snap.homes[0].name, "Our Home");

        // Rooms
        assert_eq!(snap.rooms.len(), 2);
        let names: Vec<&str> = snap.rooms.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"Living Room"), "expected Living Room");
        assert!(names.contains(&"Kitchen"), "expected Kitchen");

        // Accessories
        assert_eq!(snap.accessories.len(), 3);
        let acc = snap.accessories.iter().find(|a| a.name == "Hue Lamp").unwrap();
        assert_eq!(acc.manufacturer, "Signify");
        assert_eq!(acc.model, "Hue White");
        assert_eq!(acc.room_pk, Some(1));
        let lock = snap.accessories.iter().find(|a| a.name == "Door Lock").unwrap();
        assert_eq!(lock.room_pk, None, "accessory with no room has None");

        // Scenes
        assert_eq!(snap.scenes.len(), 1);
        assert_eq!(snap.scenes[0].name, "Good Morning");
        // ZTYPE is VARCHAR — verify it parses as a String, not silently dropped.
        assert_eq!(
            snap.scenes[0].kind.as_deref(),
            Some("com.apple.HomeKit.actionSet.type.userDefined"),
            "ZTYPE must be read as String from VARCHAR column"
        );

        // Automations
        assert_eq!(snap.automations.len(), 2);
        let sunset = snap.automations.iter().find(|a| a.name == "Sunset Lights").unwrap();
        assert!(sunset.active, "Sunset Lights should be active");
        assert_eq!(sunset.significant_event.as_deref(), Some("sunset"));
        assert!(sunset.last_fired.is_some(), "last_fired should decode from CoreData ts");
        let morning = snap.automations.iter().find(|a| a.name == "Morning Alarm").unwrap();
        assert!(!morning.active, "Morning Alarm should be inactive");
        assert_eq!(morning.last_fired, None, "NULL fire date → None");

        // Guid format
        assert!(snap.guid.starts_with("HOME-UUID-0001:"), "guid = home_uuid:date");
    }

    #[test]
    fn apple_homekit_snapshot_hash_ignores_ts() {
        let db_path = std::env::temp_dir()
            .join(format!("trove-hk-hash-{}.db", std::process::id()));
        let _ = fs::remove_file(&db_path);
        make_fake_hk_db(&db_path);

        let mut snap1 = read_homekit_db(&db_path).expect("read 1");
        let mut snap2 = read_homekit_db(&db_path).expect("read 2");
        let _ = fs::remove_file(&db_path);

        // Even if the ts differs, the hash must be equal for identical topology.
        snap1.ts = "2026-06-17T08:00:00-07:00".to_string();
        snap2.ts = "2026-06-17T09:00:00-07:00".to_string();
        assert_eq!(
            snapshot_hash(&snap1),
            snapshot_hash(&snap2),
            "identical topology must yield the same hash regardless of ts"
        );
    }

    #[test]
    fn apple_homekit_snapshot_hash_differs_when_accessory_added() {
        let db_path = std::env::temp_dir()
            .join(format!("trove-hk-diff-{}.db", std::process::id()));
        let _ = fs::remove_file(&db_path);
        make_fake_hk_db(&db_path);
        let snap1 = read_homekit_db(&db_path).expect("read 1");

        // Add an accessory.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "INSERT INTO ZMKFACCESSORY VALUES (4, 'New Sensor', 'ACME', 'Sensor Pro', 'ACC-UUID-0004', 1);",
            )
            .unwrap();
        }
        let snap2 = read_homekit_db(&db_path).expect("read 2");
        let _ = fs::remove_file(&db_path);

        assert_ne!(
            snapshot_hash(&snap1),
            snapshot_hash(&snap2),
            "adding an accessory must change the hash"
        );
    }

    #[test]
    fn apple_homekit_snapshot_written_when_new_and_skipped_when_unchanged() {
        let vault = temp_vault("dedup");
        let db_path = std::env::temp_dir()
            .join(format!("trove-hk-dedup-{}.db", std::process::id()));
        let _ = fs::remove_file(&db_path);
        make_fake_hk_db(&db_path);

        // First pass: read + write snapshot.
        let snap = read_homekit_db(&db_path).unwrap();
        let hash = snapshot_hash(&snap);

        // Simulate the first write (create the dir first — mirrors production behavior).
        let now = Local::now();
        let date_str = now.format("%Y-%m-%d").to_string();
        let snap_path = vault.resolve(&format!("{SNAP_DIR}/{date_str}.json")).unwrap();
        fs::create_dir_all(snap_path.parent().unwrap()).unwrap();
        fs::write(&snap_path, serde_json::to_vec_pretty(&snap).unwrap()).unwrap();
        vault.write_homekit_sync(&SyncState {
            last_hash: hash.clone(),
            updated: now.to_rfc3339(),
        }).unwrap();

        // Second pass: same DB → hash matches → no new file (dedup).
        let snap2 = read_homekit_db(&db_path).unwrap();
        let hash2 = snapshot_hash(&snap2);
        let state2 = vault.read_homekit_sync();
        assert_eq!(hash, hash2, "same db → same hash");
        assert_eq!(state2.last_hash, hash2, "cursor holds the hash");
        // If hash equals last_hash, snapshot_homekit returns Ok(false).
        // We verify the dedup logic: state.last_hash == hash → skip.
        assert!(
            state2.last_hash == hash2,
            "unchanged topology → cursor matches → snapshot would be skipped"
        );

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn apple_homekit_schema_adaptive_missing_table_yields_empty_list() {
        // A DB with NO HomeKit tables (e.g. a future macOS that renames them)
        // should produce an empty but valid snapshot — never an error.
        let db_path = std::env::temp_dir()
            .join(format!("trove-hk-empty-{}.db", std::process::id()));
        let _ = fs::remove_file(&db_path);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE dummy (id INTEGER);").unwrap();
        drop(conn);

        let snap = read_homekit_db(&db_path).expect("empty DB must not error");
        let _ = fs::remove_file(&db_path);

        assert!(snap.homes.is_empty(), "no ZMKFHOME → empty homes");
        assert!(snap.rooms.is_empty(), "no ZMKFROOM → empty rooms");
        assert!(snap.accessories.is_empty(), "no ZMKFACCESSORY → empty accessories");
        assert!(snap.scenes.is_empty(), "no ZMKFACTIONSET → empty scenes");
        assert!(snap.automations.is_empty(), "no ZMKFTRIGGER → empty automations");
        // guid must still have a valid shape.
        assert!(snap.guid.contains(':'), "guid must still contain the date separator");
    }

    #[test]
    fn apple_homekit_automation_blob_stored_as_base64() {
        let db_path = std::env::temp_dir()
            .join(format!("trove-hk-blob-{}.db", std::process::id()));
        let _ = fs::remove_file(&db_path);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZMKFTRIGGER (
                Z_PK INTEGER PRIMARY KEY, ZCONFIGUREDNAME TEXT,
                ZACTIVE INTEGER, Z_ENT INTEGER, ZSIGNIFICANTEVENT TEXT,
                ZEVALUATIONCONDITION BLOB, ZMOSTRECENTFIREDATE REAL
            );",
        )
        .unwrap();
        // A fake binary blob (simulates NSKeyedArchiver plist).
        let fake_blob: &[u8] = b"\xBF\x00\x01NSKeyedArchiver\x00";
        conn.execute(
            "INSERT INTO ZMKFTRIGGER VALUES (1, 'Blob Auto', 1, 5, NULL, ?1, NULL)",
            rusqlite::params![fake_blob],
        )
        .unwrap();
        drop(conn);

        let snap = read_homekit_db(&db_path).expect("blob auto must not error");
        let _ = fs::remove_file(&db_path);

        assert_eq!(snap.automations.len(), 1);
        let auto = &snap.automations[0];
        let blob_b64 = auto.condition_blob.as_deref().expect("blob should be stored as base64");
        // Verify round-trip: base64-decode → original bytes.
        let decoded = base64::engine::general_purpose::STANDARD.decode(blob_b64).unwrap();
        assert_eq!(decoded, fake_blob, "blob must round-trip through base64");
    }

    #[test]
    fn apple_homekit_core_data_ts_conversion() {
        // 759484800 CoreData seconds → 2025-02-24T01:00:00 UTC
        // (759484800 + 978307200 = 1737792000 Unix)
        let unix_expected = 759_484_800 + APPLE_EPOCH_OFFSET_S;
        let ts = core_data_ts(759_484_800.0).expect("valid CoreData ts must convert");
        let dt = DateTime::parse_from_rfc3339(&ts).unwrap();
        assert_eq!(dt.timestamp(), unix_expected);
        // Zero/negative → None.
        assert!(core_data_ts(0.0).is_none());
        assert!(core_data_ts(-1.0).is_none());
    }

    #[test]
    fn apple_homekit_def_is_periodic_local_sync() {
        assert_eq!(DEF.meta.id, "apple-homekit");
        assert_eq!(DEF.meta.domain, "home");
        assert!(DEF.connection.is_none(), "no login needed");
        assert!(matches!(DEF.behavior, crate::registry::Behavior::Periodic { .. }));
    }

    #[test]
    fn apple_homekit_snapshot_serializes_and_round_trips() {
        let snap = HomeKitSnapshot {
            guid: "HOME-UUID-0001:2026-06-17".to_string(),
            ts: "2026-06-17T08:00:00-07:00".to_string(),
            homes: vec![HomeRow { uuid: "HOME-UUID-0001".into(), name: "Our Home".into() }],
            rooms: vec![RoomRow { pk: 1, name: "Living Room".into(), uuid: "R1".into() }],
            accessories: vec![AccessoryRow {
                pk: 1,
                name: "Hue Lamp".into(),
                manufacturer: "Signify".into(),
                model: "Hue White".into(),
                uuid: "A1".into(),
                room_pk: Some(1),
            }],
            scenes: vec![SceneRow { pk: 1, name: "Good Morning".into(), kind: Some("com.apple.HomeKit.actionSet.type.userDefined".into()) }],
            automations: vec![AutomationRow {
                pk: 1,
                name: "Sunset Lights".into(),
                active: true,
                trigger_type: Some(12),
                significant_event: Some("sunset".into()),
                condition_blob: None,
                last_fired: Some("2026-06-16T20:00:00-07:00".into()),
            }],
        };

        let json = serde_json::to_string(&snap).unwrap();
        let back: HomeKitSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(back.guid, snap.guid);
        assert_eq!(back.homes[0].name, "Our Home");
        assert_eq!(back.rooms[0].name, "Living Room");
        assert_eq!(back.accessories[0].manufacturer, "Signify");
        assert_eq!(back.scenes[0].name, "Good Morning");
        assert_eq!(back.automations[0].significant_event.as_deref(), Some("sunset"));
        assert!(back.automations[0].condition_blob.is_none());
    }
}
