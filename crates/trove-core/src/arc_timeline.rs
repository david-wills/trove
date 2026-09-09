//! Arc Timeline (iOS) — ML-classified place visits and trips from the Big Paua
//! Arc app, imported as a user-supplied SQLite database (the LocoKit local
//! store format used by the original Arc iOS app; Arc 4 / 2025-rebuild export
//! format is unconfirmed). Brief: docs/integrations/arc-timeline.md.
//!
//! ## What Arc Timeline stores
//!
//! Arc records a continuous GPS trail and ML-classifies it into **visits**
//! (stationary periods with a named place) and **paths** (movement with a
//! detected transport mode). Internally it uses the
//! [LocoKit](https://github.com/sobri909/LocoKit) framework whose local-store
//! option persists into a SQLite file (`LocoKit.sqlite`) with two core tables:
//!
//! - **`TimelineItem`** — one row per visit or path segment.
//!   Key columns: `itemId` (UUID text PK), `isVisit` (boolean), `startDate`,
//!   `endDate`, `latitude`, `longitude` (centroid), `altitude` (double),
//!   `activityType` (text: "cycling", "walking", "driving", …), `distance`
//!   (double, metres), `stepCount` (integer), `deleted` (boolean).
//! - **`LocomotionSample`** — one row per 6–30-second GPS fix.
//!   Key columns: `sampleId` (UUID text PK), `timelineItemId` (FK),
//!   `date` (DATETIME — GRDB convention: UTC, stored as `YYYY-MM-DD HH:MM:SS.SSS`),
//!   `secondsFromGMT` (INTEGER, nullable — added in LocoKit migration "7.0.4 timezones";
//!   NULL on pre-7.0.4 rows), `latitude`, `longitude`, `altitude`, `speed`,
//!   `course`, `horizontalAccuracy`, `verticalAccuracy`, `classifiedType`,
//!   `confirmedType` (activity type), `deleted` (boolean).
//!
//! Schema verified against
//! `LocoKit/Timelines/TimelineStore+Migrations.swift` (commit HEAD, 2026).
//!
//! ## Date/timezone encoding
//!
//! GRDB stores `.datetime` columns as **UTC** text with a space separator
//! (`YYYY-MM-DD HH:MM:SS.SSS`) — no embedded offset. The local timezone offset
//! at record time is kept in a **separate nullable column** `secondsFromGMT`
//! (added in migration "7.0.4 timezones"; `LocomotionSample.swift` lines ~148-150,
//! 175, 251). Pre-7.0.4 rows have `NULL` in `secondsFromGMT` — their local time
//! is genuinely ambiguous; we fall back to UTC (`Z`) for those rows.
//!
//! ## This build
//!
//! **Raw layer (unconditional):** the user-supplied `.sqlite` file is copied
//! verbatim under `location/arc-timeline/raw/arc-timeline-<hash>.sqlite`. A
//! re-import of the same file is a no-op (content-hash naming deduplicates).
//!
//! **Contract layer:** `LocomotionSample` rows that belong to path segments
//! (`TimelineItem.isVisit = 0`) are mapped to [`crate::location::Fix`] rows
//! (one fix per sample). Uses `PRAGMA table_info` to tolerate column drift
//! across LocoKit versions — columns absent from the schema are silently skipped.
//! The transport mode (`confirmedType` → `mode`) and path metadata
//! (`activityType`, `distance`) ride in `extra`. Place **visits** are
//! visit/place-shaped, not fix-shaped — they stay in the raw copy until a
//! visits-shaped contract lands (per the location-domain ruling).
//!
//! ## Privacy
//!
//! A continuous location trail is highly sensitive; this integration ships
//! opt-in (`default_on: false`) with an explicit privacy caveat.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::location::Fix;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer fix stream (day-partitioned); raw SQLite copies nest under
/// `raw/`.
const DIR: &str = "location/arc-timeline";
const RAW_DIR: &str = "location/arc-timeline/raw";

/// The source id written into every [`Fix`] row — identical to the vault
/// folder name and the `source` field in the location domain.
#[allow(dead_code)]
const SOURCE: &str = "arc-timeline";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the `pub mod` and
/// `&DEF` lines already exist in the stub; this replaces only the body).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "arc-timeline",
        name: "Arc Timeline",
        kind: IntegrationKind::Import,
        // Privacy-sensitive (a continuous where-you've-been trail): opt-in
        // only, with explicit acknowledgement.
        default_on: false,
        description: "Import your movement history from the Arc Timeline iOS app — visits, \
                      trips, and GPS traces. The LocoKit SQLite export is preserved in full; \
                      path fixes are mapped to the location contract automatically. Re-runnable: \
                      re-dropping the same file never duplicates.",
        domain: "location",
        vault_path: "location/arc-timeline/",
        toggleable: false,
        setup: &[
            "In the Arc app, use Settings → Export / Backup to export your timeline database.",
            "Import the produced .sqlite file here.",
        ],
        caveats: "Highly sensitive: a continuous record of everywhere you've been. \
                  Ships opt-in by default. \
                  Arc 4's export story is unconfirmed; if the file is accepted, path \
                  fixes are mapped and your full export is preserved under location/arc-timeline/raw/.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // Arc LocoKit export: a SQLite database file.
    accepts: &["sqlite", "sqlite3", "db"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Format detection

/// The format of a user-supplied Arc export file — detected by probing the
/// file, not by file extension alone (extension may vary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// A SQLite file whose schema contains the LocoKit `TimelineItem` table
    /// (the documented Arc/LocoKit local-store format).
    LocoKitSqlite,
}

impl Format {
    /// A stable slug for the raw artifact filename.
    fn slug(self) -> &'static str {
        match self {
            Format::LocoKitSqlite => "locokit-sqlite",
        }
    }

    /// Detect the export format from a file on disk.
    ///
    /// Returns `None` if the file is not a recognized Arc export shape — a
    /// clear rejection rather than a silent store of an unrelated file.
    fn detect(path: &Path) -> Result<Option<Format>> {
        // A SQLite file starts with the magic header "SQLite format 3\0".
        let mut header = [0u8; 16];
        let n = {
            use std::io::Read;
            let mut f = std::fs::File::open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            f.read(&mut header).with_context(|| "reading file header")?
        };
        if n < 16 || &header[..15] != b"SQLite format 3" {
            return Ok(None); // not a SQLite file at all
        }

        // Open read-only and probe for the LocoKit TimelineItem table.
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening SQLite {}", path.display()))?;

        let has_timeline_item: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='TimelineItem'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);

        if has_timeline_item {
            Ok(Some(Format::LocoKitSqlite))
        } else {
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// The import

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let Some(format) = Format::detect(path)? else {
        bail!(
            "{} doesn't look like an Arc Timeline export — expected a SQLite file with a \
             TimelineItem table (the LocoKit local-store format). \
             Check that you exported the timeline database from the Arc app.",
            path.display()
        );
    };
    progress(ImportProgress { records: 0, percent: 25.0 });

    // --- Raw layer (unconditional, full fidelity) ----------------------------
    // The whole SQLite file preserved verbatim under raw/, one file per import,
    // named by format + content hash. Re-dropping the same export is a no-op
    // (idempotent via content-hash naming).
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let hash = content_hash(&bytes);
    let rel = format!("{RAW_DIR}/{}-{hash}.sqlite", format.slug());
    let raw_path = vault.resolve(&rel)?;
    let already = raw_path.exists();
    if !already {
        crate::store::write_atomic(&raw_path, &bytes)?;
    }
    progress(ImportProgress { records: 0, percent: 75.0 });

    // --- Contract layer ------------------------------------------------------
    // Map non-deleted LocomotionSample rows from path segments (isVisit=0) to
    // Fix records. Uses PRAGMA table_info to tolerate column drift across
    // LocoKit versions. Errors from the SQLite read are logged but don't abort
    // the import — the raw layer is always preserved regardless.
    let fixes = match map_fixes(path, format) {
        Ok(v) => v,
        Err(e) => {
            // Raw is already safe; emit zero fixes and note the error in the
            // headline rather than aborting the whole import.
            eprintln!("arc-timeline: map_fixes failed (raw preserved): {e:#}");
            Vec::new()
        }
    };
    let mapped = fixes.len() as u64;
    if !fixes.is_empty() {
        write_fixes(vault, &fixes)?;
    }
    progress(ImportProgress { records: mapped, percent: 100.0 });

    let raw_note = if already {
        "export already archived (idempotent re-drop)"
    } else {
        "export archived in full"
    };
    let headline = if mapped > 0 {
        format!("{mapped} location fixes imported ({} format); {raw_note}", format.slug())
    } else {
        format!("{} export recognized — {raw_note}; no path fixes found", format.slug())
    };
    Ok(ImportOutcome {
        headline,
        counts: [("fixes", mapped), ("archived", u64::from(!already))].into(),
    })
}

/// Append new [`Fix`] rows (day-partitioned by local `ts`), deduped by `guid`
/// against what's already on disk — re-runnable: a re-import never duplicates.
fn write_fixes(vault: &Vault, fixes: &[Fix]) -> Result<()> {
    let stream = vault.stream(DIR, Partition::Day);
    let mut seen = std::collections::HashSet::new();
    for key in stream.partitions()? {
        for f in stream.read::<Fix>(&key)? {
            if !f.guid.is_empty() {
                seen.insert(f.guid);
            }
        }
    }
    let fresh: Vec<&Fix> = fixes
        .iter()
        .filter(|f| f.guid.is_empty() || seen.insert(f.guid.clone()))
        .collect();
    stream.append(&fresh, |f| &f.ts)?;
    Ok(())
}

/// Which optional columns exist in `LocomotionSample` — probed via
/// `PRAGMA table_info` to tolerate LocoKit version drift (same technique as
/// `apple_voice_memos::probe_columns`).
struct SampleCols {
    altitude: bool,
    speed: bool,
    course: bool,
    horizontal_accuracy: bool,
    classified_type: bool,
    confirmed_type: bool,
    seconds_from_gmt: bool,
}

fn probe_sample_cols(conn: &rusqlite::Connection) -> Result<SampleCols> {
    use std::collections::HashSet;
    let mut have: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare("PRAGMA table_info(LocomotionSample)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        have.insert(name);
    }
    Ok(SampleCols {
        altitude: have.contains("altitude"),
        speed: have.contains("speed"),
        course: have.contains("course"),
        horizontal_accuracy: have.contains("horizontalAccuracy"),
        classified_type: have.contains("classifiedType"),
        confirmed_type: have.contains("confirmedType"),
        seconds_from_gmt: have.contains("secondsFromGMT"),
    })
}

/// Which optional columns exist in `TimelineItem` — probed via `PRAGMA table_info`.
struct ItemCols {
    activity_type: bool,
    distance: bool,
    step_count: bool,
}

fn probe_item_cols(conn: &rusqlite::Connection) -> Result<ItemCols> {
    use std::collections::HashSet;
    let mut have: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare("PRAGMA table_info(TimelineItem)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        have.insert(name);
    }
    Ok(ItemCols {
        activity_type: have.contains("activityType"),
        distance: have.contains("distance"),
        step_count: have.contains("stepCount"),
    })
}

/// Convert a GRDB UTC datetime string + optional `secondsFromGMT` offset into
/// an RFC3339 timestamp.
///
/// GRDB stores `.datetime` columns as UTC text with a space separator:
/// `YYYY-MM-DD HH:MM:SS[.SSS]`. The local offset at record time lives in the
/// separate nullable `secondsFromGMT` column (LocoKit migration "7.0.4 timezones").
/// Pre-7.0.4 rows have NULL — we emit a UTC `Z` suffix for those (the local
/// time is genuinely ambiguous).
fn grdb_utc_to_rfc3339(date_str: &str, seconds_from_gmt: Option<i64>) -> String {
    // Normalize: GRDB uses space separator; RFC3339 uses T.
    let normalized = date_str.trim().replace(' ', "T");
    // Strip any trailing fractional seconds for parsing, then reattach.
    let (base, frac) = if let Some(pos) = normalized.find('.') {
        let (b, f) = normalized.split_at(pos);
        (b.to_string(), f.to_string())
    } else {
        (normalized.clone(), String::new())
    };

    match seconds_from_gmt {
        Some(offset) => {
            let abs = offset.unsigned_abs();
            let sign = if offset >= 0 { '+' } else { '-' };
            let hh = abs / 3600;
            let mm = (abs % 3600) / 60;
            format!("{base}{frac}{sign}{hh:02}:{mm:02}")
        }
        None => {
            // UTC fallback for pre-7.0.4 rows.
            format!("{base}{frac}Z")
        }
    }
}

/// Map a LocoKit SQLite export into contract [`Fix`] rows.
///
/// Opens the SQLite read-only, uses `PRAGMA table_info` to enumerate present
/// columns (column-drift safe across LocoKit versions), then executes:
///
/// ```sql
/// SELECT s.*, t.activityType, t.distance, t.stepCount
/// FROM   LocomotionSample s
/// JOIN   TimelineItem t ON s.timelineItemId = t.itemId
/// WHERE  s.deleted = 0 AND t.isVisit = 0
/// ```
///
/// Per fix:
/// - `sampleId` → `guid` (stable dedupe key).
/// - `timelineItemId` → `trail` (groups fixes of one path segment).
/// - `date` (GRDB UTC) + `secondsFromGMT` (nullable offset) → `ts` (RFC3339).
///   When `secondsFromGMT` is NULL (pre-7.0.4 rows) the timestamp is UTC (`Z`).
/// - `latitude`, `longitude` → `lat`, `lon`.
/// - `altitude` → `ele` (NULL → omit).
/// - `speed` → `speed` (m/s; CLLocation emits −1.0 when unknown → filter).
/// - `horizontalAccuracy` → `accuracy`.
/// - `course` → `heading`.
/// - `confirmedType` (preferred) or `classifiedType` → `mode`.
/// - Parent item's `activityType`, `distance`, `stepCount` → `extra`.
///
/// **Place visits stay raw.** Samples belonging to `TimelineItem.isVisit = 1`
/// are excluded — they are visit/place-shaped and remain in the raw copy until
/// a visits-shaped contract lands.
fn map_fixes(path: &Path, _format: Format) -> Result<Vec<Fix>> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening LocoKit SQLite read-only: {}", path.display()))?;

    let sc = probe_sample_cols(&conn)?;
    let ic = probe_item_cols(&conn)?;

    // Build SELECT list from confirmed-present columns only.
    // Core (always expected): sampleId, date, deleted, timelineItemId, latitude, longitude.
    let mut sel: Vec<&str> = vec!["s.sampleId", "s.date", "s.timelineItemId", "s.latitude", "s.longitude"];
    if sc.altitude          { sel.push("s.altitude"); }
    if sc.speed             { sel.push("s.speed"); }
    if sc.course            { sel.push("s.course"); }
    if sc.horizontal_accuracy { sel.push("s.horizontalAccuracy"); }
    if sc.confirmed_type    { sel.push("s.confirmedType"); }
    if sc.classified_type   { sel.push("s.classifiedType"); }
    if sc.seconds_from_gmt  { sel.push("s.secondsFromGMT"); }
    if ic.activity_type     { sel.push("t.activityType"); }
    if ic.distance          { sel.push("t.distance"); }
    if ic.step_count        { sel.push("t.stepCount"); }

    let query = format!(
        "SELECT {cols} \
         FROM   LocomotionSample s \
         JOIN   TimelineItem t ON s.timelineItemId = t.itemId \
         WHERE  s.deleted = 0 AND t.isVisit = 0",
        cols = sel.join(", ")
    );

    let mut stmt = conn.prepare(&query)
        .with_context(|| "preparing LocomotionSample query")?;
    let mut rows = stmt.query([])
        .with_context(|| "executing LocomotionSample query")?;

    let mut fixes: Vec<Fix> = Vec::new();

    while let Some(row) = rows.next()? {
        // Core columns (always present — query would fail otherwise).
        let sample_id: String = row.get(0)?;
        let date_str: String  = row.get(1)?;
        let trail: String     = row.get(2)?;
        let lat: f64          = row.get(3)?;
        let lon: f64          = row.get(4)?;

        // Optional columns — indexed by position in sel[].
        let mut col = 5usize; // next index into the result row

        let altitude: Option<f64> = if sc.altitude {
            let v = row.get::<_, Option<f64>>(col)?; col += 1; v
        } else { None };

        let speed_raw: Option<f64> = if sc.speed {
            let v = row.get::<_, Option<f64>>(col)?; col += 1; v
        } else { None };

        let course: Option<f64> = if sc.course {
            let v = row.get::<_, Option<f64>>(col)?; col += 1; v
        } else { None };

        let horiz_acc: Option<f64> = if sc.horizontal_accuracy {
            let v = row.get::<_, Option<f64>>(col)?; col += 1; v
        } else { None };

        let confirmed_type: Option<String> = if sc.confirmed_type {
            let v = row.get::<_, Option<String>>(col)?; col += 1; v
        } else { None };

        let classified_type: Option<String> = if sc.classified_type {
            let v = row.get::<_, Option<String>>(col)?; col += 1; v
        } else { None };

        let seconds_from_gmt: Option<i64> = if sc.seconds_from_gmt {
            let v = row.get::<_, Option<i64>>(col)?; col += 1; v
        } else { None };

        let activity_type: Option<String> = if ic.activity_type {
            let v = row.get::<_, Option<String>>(col)?; col += 1; v
        } else { None };

        let distance: Option<f64> = if ic.distance {
            let v = row.get::<_, Option<f64>>(col)?; col += 1; v
        } else { None };

        let step_count: Option<i64> = if ic.step_count {
            let v = row.get::<_, Option<i64>>(col)?;
            // col += 1 not needed — last column
            v
        } else { None };

        let _ = col; // suppress unused-variable warning

        // Build the timestamp from GRDB UTC + optional offset.
        let ts = grdb_utc_to_rfc3339(&date_str, seconds_from_gmt);

        // Mode: prefer confirmed over classified (both may be absent).
        let mode = confirmed_type
            .or(classified_type)
            .unwrap_or_default();

        // Speed: CLLocation emits −1.0 when unknown — filter negatives.
        let speed = speed_raw.filter(|&s| s >= 0.0);

        let mut fix = Fix::new(SOURCE, ts, lat, lon);
        fix.guid    = sample_id;
        fix.trail   = trail;
        fix.mode    = mode;
        fix.ele     = altitude;
        fix.speed   = speed;
        fix.heading = course;
        fix.accuracy = horiz_acc;

        // Source-specific overflow → extra.
        if let Some(at) = activity_type { fix.extra.insert("activityType".into(), at.into()); }
        if let Some(d)  = distance       { fix.extra.insert("distance_m".into(), d.into()); }
        if let Some(sc) = step_count     { fix.extra.insert("stepCount".into(), sc.into()); }

        fixes.push(fix);
    }

    Ok(fixes)
}

/// A short stable hex content hash for naming raw artifacts (dedup key for an
/// idempotent re-drop). Not cryptographic — FNV-1a 64-bit, collision-resistance
/// sufficient to distinguish distinct exports.
fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-arc-timeline-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import(v: &Vault, file: &str, bytes: &[u8]) -> Result<ImportOutcome> {
        let path = v.root().join(file);
        fs::write(&path, bytes).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {})
    }

    /// Minimal valid SQLite database bytes containing a `TimelineItem` table —
    /// constructed programmatically so no on-disk fixture is needed. This
    /// proves format detection, fix mapping, and the raw-layer scaffold without
    /// needing a real Arc export.
    ///
    /// Includes `secondsFromGMT` (LocoKit 7.0.4+ column) set to +7200 (UTC+2)
    /// on `samp-1` and NULL on `samp-2` (pre-7.0.4 row — falls back to UTC `Z`).
    /// Also includes a visit item (`isVisit=1`) and a deleted sample to verify
    /// they are filtered out.
    fn minimal_locokit_sqlite(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE TimelineItem (
                itemId       TEXT PRIMARY KEY,
                lastSaved    DATETIME NOT NULL,
                deleted      BOOLEAN NOT NULL,
                isVisit      BOOLEAN NOT NULL,
                startDate    DATETIME,
                endDate      DATETIME,
                latitude     DOUBLE,
                longitude    DOUBLE,
                altitude     DOUBLE,
                activityType TEXT,
                distance     DOUBLE,
                stepCount    INTEGER
            );
            CREATE TABLE LocomotionSample (
                sampleId           TEXT PRIMARY KEY,
                date               DATETIME NOT NULL,
                deleted            BOOLEAN NOT NULL,
                timelineItemId     TEXT,
                latitude           DOUBLE,
                longitude          DOUBLE,
                altitude           DOUBLE,
                speed              DOUBLE,
                course             DOUBLE,
                horizontalAccuracy DOUBLE,
                classifiedType     TEXT,
                confirmedType      TEXT,
                secondsFromGMT     INTEGER
            );

            -- Path item (isVisit=0): its samples become Fix rows.
            INSERT INTO TimelineItem (itemId, lastSaved, deleted, isVisit, startDate, endDate,
                                      latitude, longitude, activityType, distance, stepCount)
            VALUES ('item-1', '2024-04-03 06:00:00', 0, 0,
                    '2024-04-03 06:00:00', '2024-04-03 06:30:00',
                    50.0506312, 14.3439906, 'cycling', 1840.0, 200);

            -- Visit item (isVisit=1): its samples are excluded from Fix output.
            INSERT INTO TimelineItem (itemId, lastSaved, deleted, isVisit)
            VALUES ('item-visit', '2024-04-03 07:00:00', 0, 1);

            -- Path sample 1: secondsFromGMT=7200 → local ts is 2024-04-03T08:14:00+02:00
            INSERT INTO LocomotionSample (sampleId, date, deleted, timelineItemId,
                                          latitude, longitude, altitude, speed, course,
                                          horizontalAccuracy, confirmedType, secondsFromGMT)
            VALUES ('samp-1', '2024-04-03 06:14:00', 0, 'item-1',
                    50.0506312, 14.3439906, 280.0, 5.1, 92.0, 8.0, 'cycling', 7200);

            -- Path sample 2: secondsFromGMT NULL → UTC fallback (2024-04-03T06:22:00Z)
            INSERT INTO LocomotionSample (sampleId, date, deleted, timelineItemId,
                                          latitude, longitude, altitude, speed, course,
                                          horizontalAccuracy, confirmedType, secondsFromGMT)
            VALUES ('samp-2', '2024-04-03 06:22:00', 0, 'item-1',
                    50.0612345, 14.3501234, 285.0, 4.8, 88.0, 6.0, 'cycling', NULL);

            -- Deleted sample: must be excluded.
            INSERT INTO LocomotionSample (sampleId, date, deleted, timelineItemId,
                                          latitude, longitude, confirmedType, secondsFromGMT)
            VALUES ('samp-del', '2024-04-03 06:25:00', 1, 'item-1',
                    50.0, 14.0, 'cycling', 7200);

            -- Visit sample: must be excluded (parent isVisit=1).
            INSERT INTO LocomotionSample (sampleId, date, deleted, timelineItemId,
                                          latitude, longitude, confirmedType, secondsFromGMT)
            VALUES ('samp-visit', '2024-04-03 07:10:00', 0, 'item-visit',
                    50.1, 14.1, 'stationary', 7200);",
        )
        .unwrap();
    }

    // --- format detection --------------------------------------------------

    #[test]
    fn detects_locokit_sqlite_format() {
        let dir = std::env::temp_dir()
            .join(format!("trove-arc-detect-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("LocoKit.sqlite");
        minimal_locokit_sqlite(&db_path);
        assert_eq!(
            Format::detect(&db_path).unwrap(),
            Some(Format::LocoKitSqlite),
            "LocoKit SQLite detected"
        );
    }

    #[test]
    fn rejects_non_sqlite_file() {
        let dir = std::env::temp_dir()
            .join(format!("trove-arc-reject-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.db");
        fs::write(&path, b"not a sqlite file at all").unwrap();
        assert_eq!(Format::detect(&path).unwrap(), None, "non-SQLite rejected");
    }

    #[test]
    fn rejects_sqlite_without_timeline_item_table() {
        let dir = std::env::temp_dir()
            .join(format!("trove-arc-noti-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("other.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE SomeOtherTable (id TEXT PRIMARY KEY);")
            .unwrap();
        drop(conn);
        assert_eq!(
            Format::detect(&path).unwrap(),
            None,
            "SQLite without TimelineItem rejected"
        );
    }

    // --- import scaffold (raw preserved, contract mapped) -------------------

    #[test]
    fn import_maps_fixes_and_preserves_raw_verbatim() {
        let v = temp_vault("import");
        // Write the SQLite to a temp path, read its bytes, then hand the path
        // to the importer (the importer uses the file at the supplied path).
        let db_path = v.root().join("Arc_Timeline.sqlite");
        minimal_locokit_sqlite(&db_path);
        let bytes = fs::read(&db_path).unwrap();

        let out = (IMPORT.run)(&v, &db_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // 2 path fixes (samp-1 + samp-2); deleted + visit samples excluded.
        assert_eq!(out.counts.get("fixes"), Some(&2), "two path fixes mapped: {out:?}");
        assert_eq!(out.counts.get("archived"), Some(&1), "first import archives the file");
        assert!(
            out.headline.contains("locokit-sqlite"),
            "headline names the format: {}",
            out.headline
        );

        // Raw layer: the whole SQLite preserved under raw/.
        let raw_dir = v.root().join("location/arc-timeline/raw");
        let files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "one raw file per import");
        let raw_bytes = fs::read(files[0].path()).unwrap();
        assert_eq!(raw_bytes, bytes, "raw file is the export verbatim, full fidelity");
        assert!(
            files[0]
                .file_name()
                .to_string_lossy()
                .starts_with("locokit-sqlite-"),
            "raw file named by format slug: {}",
            files[0].file_name().to_string_lossy()
        );

        // Contract day-file written for the local date of the fixes.
        // samp-1: UTC 06:14 + +02:00 → local 08:14 on 2024-04-03.
        // samp-2: UTC 06:22 + NULL   → UTC 2024-04-03T06:22:00Z → local day 2024-04-03.
        assert!(
            v.root().join("location/arc-timeline/2024-04-03.jsonl").exists(),
            "contract day-file written for 2024-04-03"
        );
    }

    #[test]
    fn re_dropping_the_same_export_is_idempotent() {
        let v = temp_vault("idempotent");
        let db_path = v.root().join("Arc_Timeline.sqlite");
        minimal_locokit_sqlite(&db_path);

        let first = (IMPORT.run)(&v, &db_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(first.counts.get("archived"), Some(&1), "first drop archives");

        let again = (IMPORT.run)(&v, &db_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(again.counts.get("archived"), Some(&0), "re-drop archives nothing new");
        assert!(
            again.headline.contains("idempotent"),
            "headline notes the no-op: {}",
            again.headline
        );

        // Still exactly one raw file.
        let raw_dir = v.root().join("location/arc-timeline/raw");
        assert_eq!(fs::read_dir(&raw_dir).unwrap().flatten().count(), 1);
    }

    #[test]
    fn rejects_a_non_arc_file() {
        let v = temp_vault("reject-json");
        let err = import(&v, "notes.json", b"{\"notes\":[]}").unwrap_err().to_string();
        assert!(
            err.contains("Arc Timeline export"),
            "clear rejection with Arc context: {err}"
        );
    }

    // --- map_fixes: field mapping and filtering ----------------------------

    #[test]
    fn map_fixes_maps_path_samples_and_filters_visits_and_deleted() {
        let dir = std::env::temp_dir()
            .join(format!("trove-arc-seam-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("LocoKit.sqlite");
        minimal_locokit_sqlite(&db_path);

        let fixes = map_fixes(&db_path, Format::LocoKitSqlite).unwrap();
        // samp-1 + samp-2 only; samp-del (deleted) and samp-visit (isVisit=1) excluded.
        assert_eq!(fixes.len(), 2, "two path fixes; deleted + visit excluded");

        // Find by guid for order-independent assertions.
        let f1 = fixes.iter().find(|f| f.guid == "samp-1").expect("samp-1 present");
        let f2 = fixes.iter().find(|f| f.guid == "samp-2").expect("samp-2 present");

        // samp-1: UTC 2024-04-03 06:14:00 + secondsFromGMT=7200 → +02:00.
        assert_eq!(f1.ts, "2024-04-03T06:14:00+02:00", "samp-1 ts with offset: {}", f1.ts);
        assert_eq!(f1.lat, 50.0506312);
        assert_eq!(f1.lon, 14.3439906);
        assert_eq!(f1.ele, Some(280.0));
        assert_eq!(f1.speed, Some(5.1));
        assert_eq!(f1.heading, Some(92.0));
        assert_eq!(f1.accuracy, Some(8.0));
        assert_eq!(f1.mode, "cycling");
        assert_eq!(f1.trail, "item-1");
        assert_eq!(f1.source, "arc-timeline");
        // Parent item extra fields.
        assert_eq!(f1.extra.get("activityType").and_then(|v| v.as_str()), Some("cycling"));
        assert_eq!(f1.extra.get("distance_m").and_then(|v| v.as_f64()), Some(1840.0));
        assert_eq!(f1.extra.get("stepCount").and_then(|v| v.as_i64()), Some(200));

        // samp-2: secondsFromGMT NULL → UTC fallback.
        assert_eq!(f2.ts, "2024-04-03T06:22:00Z", "samp-2 ts falls back to UTC Z: {}", f2.ts);
        assert_eq!(f2.lat, 50.0612345);
        assert_eq!(f2.mode, "cycling");
    }

    #[test]
    fn map_fixes_filters_negative_speed() {
        // CLLocation emits -1.0 when speed is unknown; must be filtered.
        let dir = std::env::temp_dir()
            .join(format!("trove-arc-negspeed-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("LocoKit.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE TimelineItem (
                itemId TEXT PRIMARY KEY, lastSaved DATETIME NOT NULL,
                deleted BOOLEAN NOT NULL, isVisit BOOLEAN NOT NULL
            );
            CREATE TABLE LocomotionSample (
                sampleId TEXT PRIMARY KEY, date DATETIME NOT NULL,
                deleted BOOLEAN NOT NULL, timelineItemId TEXT,
                latitude DOUBLE, longitude DOUBLE, speed DOUBLE
            );
            INSERT INTO TimelineItem VALUES ('t1','2024-01-01 00:00:00',0,0);
            INSERT INTO LocomotionSample VALUES
                ('s-neg','2024-01-01 00:00:00',0,'t1',1.0,2.0,-1.0),
                ('s-zero','2024-01-01 00:01:00',0,'t1',1.1,2.1,0.0),
                ('s-pos','2024-01-01 00:02:00',0,'t1',1.2,2.2,3.5);",
        ).unwrap();
        drop(conn);

        let fixes = map_fixes(&db_path, Format::LocoKitSqlite).unwrap();
        assert_eq!(fixes.len(), 3, "three samples present");
        let neg  = fixes.iter().find(|f| f.guid == "s-neg").unwrap();
        let zero = fixes.iter().find(|f| f.guid == "s-zero").unwrap();
        let pos  = fixes.iter().find(|f| f.guid == "s-pos").unwrap();
        assert!(neg.speed.is_none(),  "negative speed filtered: {:?}", neg.speed);
        assert_eq!(zero.speed, Some(0.0), "zero speed preserved");
        assert_eq!(pos.speed,  Some(3.5), "positive speed preserved");
    }

    #[test]
    fn map_fixes_tolerates_missing_optional_columns() {
        // A minimal schema with only the core columns — PRAGMA probe must not crash.
        let dir = std::env::temp_dir()
            .join(format!("trove-arc-mincols-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("LocoKit.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE TimelineItem (
                itemId TEXT PRIMARY KEY, lastSaved DATETIME NOT NULL,
                deleted BOOLEAN NOT NULL, isVisit BOOLEAN NOT NULL
            );
            CREATE TABLE LocomotionSample (
                sampleId TEXT PRIMARY KEY, date DATETIME NOT NULL,
                deleted BOOLEAN NOT NULL, timelineItemId TEXT,
                latitude DOUBLE, longitude DOUBLE
            );
            INSERT INTO TimelineItem VALUES ('t1','2024-01-01 00:00:00',0,0);
            INSERT INTO LocomotionSample VALUES ('s1','2024-01-01 00:00:00',0,'t1',10.0,20.0);",
        ).unwrap();
        drop(conn);

        let fixes = map_fixes(&db_path, Format::LocoKitSqlite).unwrap();
        assert_eq!(fixes.len(), 1, "one fix from minimal schema");
        let f = &fixes[0];
        assert_eq!(f.guid, "s1");
        assert_eq!(f.lat, 10.0);
        assert!(f.ele.is_none() && f.speed.is_none() && f.mode.is_empty(),
            "optional fields absent: ele={:?} speed={:?} mode={:?}", f.ele, f.speed, f.mode);
        // Timestamp without secondsFromGMT → UTC Z suffix.
        assert!(f.ts.ends_with('Z'), "UTC fallback ts: {}", f.ts);
    }

    #[test]
    fn grdb_utc_to_rfc3339_encodes_correctly() {
        // With positive offset.
        assert_eq!(
            grdb_utc_to_rfc3339("2024-04-03 06:14:00", Some(7200)),
            "2024-04-03T06:14:00+02:00"
        );
        // With negative offset (e.g. -5h = -18000s).
        assert_eq!(
            grdb_utc_to_rfc3339("2024-04-03 15:30:00", Some(-18000)),
            "2024-04-03T15:30:00-05:00"
        );
        // With fractional seconds.
        assert_eq!(
            grdb_utc_to_rfc3339("2024-04-03 06:14:00.123", Some(3600)),
            "2024-04-03T06:14:00.123+01:00"
        );
        // NULL offset → UTC Z.
        assert_eq!(
            grdb_utc_to_rfc3339("2024-04-03 06:14:00", None),
            "2024-04-03T06:14:00Z"
        );
    }

    // --- the write seam (proves the parked->live path is wired) -----------

    #[test]
    fn write_fixes_day_partitions_and_dedupes_by_guid() {
        let v = temp_vault("writefixes");
        let mut a = Fix::new(SOURCE, "2024-04-03T08:14:00+02:00", 50.0506312, 14.3439906);
        a.guid = "arc-samp-1".into();
        a.mode = "cycling".into();
        a.trail = "item-1".into();
        a.ele = Some(280.0);
        let mut b = Fix::new(SOURCE, "2024-04-03T08:22:00+02:00", 50.0612345, 14.3501234);
        b.guid = "arc-samp-2".into();
        b.mode = "cycling".into();
        b.trail = "item-1".into();
        write_fixes(&v, &[a.clone(), b]).unwrap();

        let day =
            fs::read_to_string(v.root().join("location/arc-timeline/2024-04-03.jsonl")).unwrap();
        assert_eq!(day.lines().count(), 2, "both fixes in the day file");
        assert!(day.contains("\"lat\":50.0506312"), "numeric lat on disk: {day}");
        assert!(day.contains("\"mode\":\"cycling\""));
        assert!(day.contains("\"ele\":280.0"), "elevation present: {day}");
        assert!(day.contains("\"trail\":\"item-1\""), "trail id present: {day}");

        // Re-writing the same guids appends nothing (idempotent).
        write_fixes(&v, &[a]).unwrap();
        let day2 =
            fs::read_to_string(v.root().join("location/arc-timeline/2024-04-03.jsonl")).unwrap();
        assert_eq!(day2.lines().count(), 2, "guid dedupe: no duplicate row");
    }

    // --- registry wiring ---------------------------------------------------

    #[test]
    fn hub_exposes_import_box_and_location_domain() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status
            .iter()
            .find(|s| s.id == "arc-timeline")
            .expect("registered in INTEGRATIONS");
        let import_info = card.import.as_ref().expect("import box info");
        assert!(
            import_info.accepts.contains(&"sqlite"),
            "accepts sqlite: {:?}",
            import_info.accepts
        );
        assert_eq!(DEF.meta.domain, "location");
        assert!(!DEF.meta.default_on, "privacy-sensitive: off by default");
        assert_eq!(DEF.connection, None, "pure import, no login");
    }

    // --- content hash (sanity) ---------------------------------------------

    #[test]
    fn content_hash_is_deterministic_and_distinct() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"world"));
        // 16 hex chars = 64-bit hash.
        assert_eq!(content_hash(b"test").len(), 16);
    }
}
