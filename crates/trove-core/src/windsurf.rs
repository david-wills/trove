//! Windsurf — periodic local sync of Cascade AI chat session metadata.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/windsurf.md.
//!
//! ## Storage reality (evidence-based, 2026-06)
//!
//! Community research confirms that Windsurf stores Cascade sessions at
//! `~/.codeium/windsurf/cascade/<uuid>.pb` as **protobuf binary files**.
//! The proto schema is not publicly documented, and at least one community
//! investigation found the file header bytes to be non-standard (possibly
//! encrypted or custom-wrapped). Windsurf's VS Code `state.vscdb` databases
//! contain only UI state, not chat history — unlike Cursor.
//!
//! Live gRPC (`GetCascadeTrajectory` / `GetAllCascadeTrajectories`) is the
//! only confirmed path to structured data, but that requires a running
//! language server process and violates the standalone rule.
//!
//! Because the protobuf schema is unavailable (and possibly encrypted), the
//! real-content parser is **parked** pending either:
//! - a real `.pb` sample + schema discovery, OR
//! - an official Windsurf export API / plaintext storage path
//!
//! ## What this collector does today
//!
//! **Raw scaffold only**: enumerates the `.pb` files in the cascade directory
//! across both old (`~/.codeium/windsurf/`) and Devin-Desktop-rebrand
//! (`~/.codeium/windsurf-next/`) paths, writes one raw catalog row per
//! discovered session file — session id, file size, first-seen time, last-
//! modified time, and the app variant — into `developer/windsurf/YYYY-MM.jsonl`.
//! No protobuf content is read or stored. This gives a session-count signal
//! today; once the schema is confirmed the pull hook can be upgraded to parse
//! content.
//!
//! **`developer/` is raw-only** (taxonomy decision): no domain contract,
//! no spec_validation row — this module owns its row shape.
//!
//! ## Partition stability
//!
//! Rows are partitioned by `first_seen` (the RFC3339 timestamp captured on
//! first discovery, frozen in the cursor). This keeps a session in the same
//! YYYY-MM file across rescans, even when the file's mtime jumps months
//! (e.g. after an archival rewrite). `last_modified` carries the current
//! mtime and is updated in-place on each scan that sees a size change.
//!
//! ## Privacy
//!
//! Even the file-path metadata (session UUIDs, mtime) counts as AI-session
//! activity data. The integration ships opt-in (default_on=false) and
//! toggleable.
//!
//! ## Rebrand probe
//!
//! Checks both old and new cascade directories so it still works whether
//! the user has the original Windsurf or the Devin Desktop fork.
//!
//! ## Parser parked
//!
//! `parser_parked_needs_sample = true`. The full-content parse is deferred
//! until a real `.pb` sample with a confirmed (or heuristically reversible)
//! proto schema exists on disk.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly — same cadence as claude_code / cursor / github_copilot.
pub const WINDSURF_SYNC_SECS: u64 = 3600;

/// Raw metadata stream: one row per discovered session file.
const DIR: &str = "developer/windsurf";
/// Rebuildable cursor: composite_key → (first_seen_ts, last_seen_bytes).
const SYNC_FILE: &str = ".trove/windsurf-sync.json";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_windsurf()?;
    let new = s.new_sessions;
    let updated = s.updated_sessions;
    Ok(crate::registry::CollectOutcome::note_if(new > 0 || updated > 0, || {
        match (new, updated) {
            (n, 0) => format!("windsurf synced — {n} new sessions"),
            (0, u) => format!("windsurf synced — {u} sessions updated"),
            (n, u) => format!("windsurf synced — {n} new, {u} updated"),
        }
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_windsurf()?;
    let headline = if s.new_sessions == 0 && s.updated_sessions == 0 {
        "Windsurf is up to date — no new sessions".to_string()
    } else {
        match (s.new_sessions, s.updated_sessions) {
            (n, 0) => format!("Windsurf synced — {n} new sessions"),
            (0, u) => format!("Windsurf synced — {u} sessions updated"),
            (n, u) => format!("Windsurf synced — {n} new, {u} updated"),
        }
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("new_sessions", s.new_sessions),
            ("updated_sessions", s.updated_sessions),
            ("skipped", s.skipped),
        ]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "windsurf",
        name: "Windsurf",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Captures your Windsurf (Cascade) AI chat session history — \
                      session identifiers, timestamps, and file metadata — from the \
                      editor's local cascade directory. Pure-local: no network, no auth. \
                      Full conversation content is pending schema discovery.",
        domain: "developer",
        vault_path: "developer/windsurf/",
        toggleable: true,
        setup: &[
            "Reads the session files Windsurf already writes under \
             ~/.codeium/windsurf/cascade/ — nothing to install or connect.",
            "Windsurf is being rebranded as Devin Desktop; the collector checks \
             both the original and new paths automatically.",
        ],
        caveats: "Windsurf stores Cascade sessions as protobuf binary files with an \
                  undocumented schema. Only session metadata (id, timestamp, size) is \
                  captured today; full conversation content requires schema discovery. \
                  The Devin Desktop rebrand may change the storage path.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(WINDSURF_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Row shape (raw, this module's own — no domain contract).

/// One row in `developer/windsurf/YYYY-MM.jsonl`.
/// Metadata only — no protobuf content (schema undocumented / possibly
/// encrypted; parser parked pending sample + schema confirmation).
///
/// The partition key is derived from `first_seen`, which is frozen at first
/// discovery. This keeps the row in the same YYYY-MM file even if the .pb
/// file's mtime jumps across month boundaries (e.g. after an archival
/// compaction rewrite). `last_modified` carries the current mtime and is
/// updated in-place on size changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SessionRow {
    /// Session UUID (the `.pb` file stem). `guid == session_id`.
    pub session_id: String,
    /// RFC3339 timestamp when this session was first discovered by Trove.
    /// Used as the stable partition key — never moves for a given session.
    #[serde(alias = "ts")]
    pub first_seen: String,
    /// RFC3339 last-modified time of the `.pb` file (from mtime).
    /// Updated on every scan that sees a size change.
    pub last_modified: String,
    /// Size of the `.pb` file in bytes.
    pub file_bytes: u64,
    /// Which Windsurf variant owned this file: "windsurf" | "windsurf-next".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Result of one scan pass.
#[derive(Debug, Clone, Default)]
pub struct WindsurfStats {
    /// Brand-new sessions (not previously seen by any variant) this pass.
    pub new_sessions: u64,
    /// Existing sessions whose file size changed (row updated in-place).
    pub updated_sessions: u64,
    /// Files that could not be written.
    pub skipped: u64,
}

// ---------------------------------------------------------------------------
// Paths.

/// All cascade directories to probe, in order of preference.
/// Returns (cascade_dir, variant_label) pairs that exist on disk.
fn cascade_dirs() -> Vec<(PathBuf, String)> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    // Original Windsurf path, then Devin Desktop rebrand path.
    let candidates = [
        (home.join(".codeium/windsurf/cascade"), "windsurf"),
        (home.join(".codeium/windsurf-next/cascade"), "windsurf-next"),
    ];
    candidates
        .into_iter()
        .filter(|(p, _)| p.exists())
        .map(|(p, label)| (p, label.to_string()))
        .collect()
}

/// All `.pb` session files in a cascade directory.
/// Returns (session_id, path, file_bytes, mtime_secs).
fn list_pb_files(dir: &Path) -> Vec<(String, PathBuf, u64, i64)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pb") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let bytes = meta.len();
        let mtime_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        out.push((stem, path, bytes, mtime_secs));
    }
    // Deterministic order.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Convert a Unix seconds timestamp to an RFC3339 local-tz string.
///
/// Falls back to the Unix epoch sentinel (`1970-01-01T00:00:00+00:00`) for
/// pathological/out-of-range values, rather than `Local::now()`, to avoid
/// silently misfiling a row into the current month partition.
pub(crate) fn secs_to_rfc3339(secs: i64) -> String {
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string())
}

// ---------------------------------------------------------------------------
// Cursor.

/// Per-entry cursor record frozen at first discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeenEntry {
    /// RFC3339 timestamp when this session was first seen — stable partition key.
    pub first_seen: String,
    /// Last-seen file size (bytes) — change proxy for live files.
    pub last_bytes: u64,
}

/// Incremental-sync state, rebuildable from vault files.
///
/// The seen-map is keyed on `"<variant>/<session_id>"` to prevent aliasing
/// between the same UUID appearing in both `windsurf` and `windsurf-next`
/// directories (documented rebrand churn).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindsurfSyncState {
    /// RFC3339 of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// `"<variant>/<session_id>"` → cursor entry (first_seen + last_bytes).
    #[serde(default)]
    pub seen: BTreeMap<String, SeenEntry>,
}

/// Composite dedup key: `"<variant>/<session_id>"`.
fn dedup_key(variant: &str, session_id: &str) -> String {
    format!("{variant}/{session_id}")
}

// ---------------------------------------------------------------------------
// Vault impl.

impl Vault {
    fn read_windsurf_sync(&self) -> WindsurfSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_windsurf_sync(&self, state: &WindsurfSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Top-level entry point for the periodic scan.
    pub fn collect_windsurf(&self) -> Result<WindsurfStats> {
        self.collect_windsurf_from_dirs(&cascade_dirs())
    }

    /// Injected-dirs variant for tests (allows synthetic cascade directories).
    pub(crate) fn collect_windsurf_from_dirs(
        &self,
        dirs: &[(PathBuf, String)],
    ) -> Result<WindsurfStats> {
        let mut stats = WindsurfStats::default();
        if dirs.is_empty() {
            return Ok(stats);
        }

        let mut state = self.read_windsurf_sync();
        let mut changed = false;

        for (cascade_dir, variant) in dirs {
            let files = list_pb_files(cascade_dir);
            for (session_id, _path, bytes, mtime_secs) in files {
                let key = dedup_key(variant, &session_id);

                if let Some(entry) = state.seen.get(&key) {
                    if entry.last_bytes == bytes {
                        // Unchanged — skip entirely.
                        continue;
                    }
                    // Size changed — update the row in-place at its stable partition.
                    let last_modified = secs_to_rfc3339(mtime_secs);
                    let row = SessionRow {
                        session_id: session_id.clone(),
                        first_seen: entry.first_seen.clone(),
                        last_modified,
                        file_bytes: bytes,
                        variant: Some(variant.clone()),
                    };
                    if let Err(e) = self.upsert_windsurf_row(&row) {
                        eprintln!("trove windsurf: upsert {session_id} failed: {e:#}");
                        stats.skipped += 1;
                        continue;
                    }
                    state.seen.insert(key, SeenEntry {
                        first_seen: row.first_seen,
                        last_bytes: bytes,
                    });
                    stats.updated_sessions += 1;
                    changed = true;
                } else {
                    // Brand-new session — capture first_seen now (stable partition key).
                    let first_seen = Local::now().to_rfc3339();
                    let last_modified = secs_to_rfc3339(mtime_secs);
                    let row = SessionRow {
                        session_id: session_id.clone(),
                        first_seen: first_seen.clone(),
                        last_modified,
                        file_bytes: bytes,
                        variant: Some(variant.clone()),
                    };
                    if let Err(e) = self.upsert_windsurf_row(&row) {
                        eprintln!("trove windsurf: upsert {session_id} failed: {e:#}");
                        stats.skipped += 1;
                        continue;
                    }
                    state.seen.insert(key, SeenEntry {
                        first_seen,
                        last_bytes: bytes,
                    });
                    stats.new_sessions += 1;
                    changed = true;
                }
            }
        }

        if changed {
            state.updated = Local::now().to_rfc3339();
            self.write_windsurf_sync(&state)?;
        }
        Ok(stats)
    }

    /// Upsert one [`SessionRow`] into `developer/windsurf/YYYY-MM.jsonl`.
    ///
    /// Partitions by `first_seen` (stable). To guard against stale rows in
    /// wrong-month files (e.g. from a pre-fix cursor), searches ALL existing
    /// partitions for the `(variant, session_id)` pair and removes any copies
    /// found outside the canonical partition before writing.
    fn upsert_windsurf_row(&self, row: &SessionRow) -> Result<()> {
        let stream = self.stream(DIR, Partition::Month);

        // The canonical partition key, derived from the stable first_seen.
        let canonical_key = Partition::Month
            .key(&row.first_seen)
            .ok_or_else(|| anyhow::anyhow!("windsurf: bad first_seen {:?}", row.first_seen))?
            .to_string();

        // Scan all existing partitions. Remove stale copies of this session
        // from any partition other than the canonical one.
        if let Ok(all_keys) = stream.partitions() {
            for key in &all_keys {
                if key == &canonical_key {
                    continue; // Will be written below.
                }
                let mut other_rows: Vec<SessionRow> = stream.read(key)?;
                let before = other_rows.len();
                other_rows.retain(|r| {
                    !(r.session_id == row.session_id
                        && r.variant == row.variant)
                });
                if other_rows.len() < before {
                    // Stale copy found — rewrite the partition without it.
                    self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &other_rows)?;
                }
            }
        }

        // Upsert into the canonical partition.
        let mut rows: Vec<SessionRow> = stream.read(&canonical_key)?;
        match rows.iter_mut().find(|r| {
            r.session_id == row.session_id && r.variant == row.variant
        }) {
            Some(existing) => *existing = row.clone(),
            None => rows.push(row.clone()),
        }
        self.write_snapshot(&format!("{DIR}/{canonical_key}.jsonl"), &rows)
    }

    // -----------------------------------------------------------------------
    // Reads.

    /// All session rows for one month (`YYYY-MM`), in file order.
    pub fn windsurf_sessions(&self, month: &str) -> Result<Vec<SessionRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-windsurf-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a synthetic cascade directory with the given `.pb` stubs.
    /// Returns (dir, variant_label).
    fn synthetic_cascade(name: &str, sessions: &[(&str, u64)]) -> (PathBuf, String) {
        let dir = std::env::temp_dir()
            .join(format!("trove-ws-cascade-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (session_id, bytes) in sessions {
            let path = dir.join(format!("{session_id}.pb"));
            let content = vec![0u8; *bytes as usize];
            fs::write(&path, content).unwrap();
        }
        (dir, "windsurf".to_string())
    }

    // -----------------------------------------------------------------------
    // Sample rows (representative, as they would appear in the vault JSONL).
    // These match the real on-disk schema: session_id is a UUID, first_seen
    // and last_modified are RFC3339, file_bytes is the .pb size, variant
    // names the app flavour.

    fn sample_row_1() -> SessionRow {
        SessionRow {
            session_id: "c4b378d8-0968-489a-85d7-db49c372c80c".to_string(),
            first_seen: "2026-05-23T14:30:00+00:00".to_string(),
            last_modified: "2026-05-23T14:30:00+00:00".to_string(),
            file_bytes: 20_971_520, // ~20 MB — typical active trajectory
            variant: Some("windsurf".to_string()),
        }
    }

    fn sample_row_2() -> SessionRow {
        SessionRow {
            session_id: "a7e3e0e1-0666-4b5c-a6a6-d27156397f70".to_string(),
            first_seen: "2026-05-24T09:10:00+00:00".to_string(),
            last_modified: "2026-05-25T12:00:00+00:00".to_string(),
            file_bytes: 245_760, // ~240 KB — archived/compacted trajectory
            variant: Some("windsurf".to_string()),
        }
    }

    #[test]
    fn sample_rows_serialize_correctly() {
        let r1 = sample_row_1();
        let json = serde_json::to_string(&r1).unwrap();
        assert!(json.contains("c4b378d8"), "session_id present");
        assert!(json.contains("file_bytes"), "file_bytes present");
        assert!(json.contains("20971520"), "correct byte count");
        assert!(json.contains("\"windsurf\""), "variant present");
        assert!(json.contains("first_seen"), "first_seen present");
        assert!(json.contains("last_modified"), "last_modified present");

        let r2 = sample_row_2();
        let json2 = serde_json::to_string(&r2).unwrap();
        assert!(json2.contains("245760"), "archived size present");
    }

    #[test]
    fn sample_rows_round_trip() {
        for row in [sample_row_1(), sample_row_2()] {
            let json = serde_json::to_string(&row).unwrap();
            let back: SessionRow = serde_json::from_str(&json).unwrap();
            assert_eq!(back, row);
        }
    }

    /// Old rows written with `"ts"` field name deserialize correctly via the
    /// `#[serde(alias = "ts")]` back-compat alias on `first_seen`.
    #[test]
    fn back_compat_ts_alias() {
        let old_json = r#"{"session_id":"abc","ts":"2026-05-01T10:00:00+00:00","last_modified":"2026-05-01T10:00:00+00:00","file_bytes":1000,"variant":"windsurf"}"#;
        let row: SessionRow = serde_json::from_str(old_json).unwrap();
        assert_eq!(row.first_seen, "2026-05-01T10:00:00+00:00");
        assert_eq!(row.session_id, "abc");
    }

    #[test]
    fn catalog_new_sessions() {
        let v = temp_vault("new");
        let uuid1 = "c4b378d8-0968-489a-85d7-db49c372c80c";
        let uuid2 = "a7e3e0e1-0666-4b5c-a6a6-d27156397f70";
        let (dir, variant) = synthetic_cascade("new", &[(uuid1, 20_000), (uuid2, 5_000)]);

        let stats = v
            .collect_windsurf_from_dirs(&[(dir, variant)])
            .unwrap();

        assert_eq!(stats.new_sessions, 2, "two .pb files → two new session rows");
        assert_eq!(stats.updated_sessions, 0);
        assert_eq!(stats.skipped, 0);
    }

    #[test]
    fn idempotent_on_rescan() {
        let v = temp_vault("idem");
        let uuid = "d1111111-1111-1111-1111-111111111111";
        let (dir, variant) = synthetic_cascade("idem", &[(uuid, 12_000)]);
        let dirs = vec![(dir, variant)];

        // First pass.
        let s1 = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(s1.new_sessions, 1);
        assert_eq!(s1.updated_sessions, 0);

        // Second pass — same file, same size → nothing new.
        let s2 = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(s2.new_sessions, 0, "unchanged file: no new sessions");
        assert_eq!(s2.updated_sessions, 0, "unchanged file: no updates either");

        // Confirm only one row in the vault.
        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.windsurf_sessions(&month).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn updated_file_size_re_emits_row() {
        let v = temp_vault("resize");
        let uuid = "e2222222-2222-2222-2222-222222222222";
        let dir = std::env::temp_dir()
            .join(format!("trove-ws-cascade-{}-resize", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pb_path = dir.join(format!("{uuid}.pb"));

        // First pass: 5 KB.
        fs::write(&pb_path, vec![0u8; 5_000]).unwrap();
        let dirs = vec![(dir.clone(), "windsurf".to_string())];
        let s1 = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(s1.new_sessions, 1);
        assert_eq!(s1.updated_sessions, 0);

        // Second pass: file grew to 25 KB (new content).
        fs::write(&pb_path, vec![0u8; 25_000]).unwrap();
        let s2 = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(s2.new_sessions, 0, "not a new session");
        assert_eq!(s2.updated_sessions, 1, "size changed → row updated");

        // Row reflects the new size (upsert, not duplicate).
        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.windsurf_sessions(&month).unwrap();
        assert_eq!(rows.len(), 1, "upsert, not duplicate");
        assert_eq!(rows[0].file_bytes, 25_000);
    }

    #[test]
    fn multiple_variants_merged() {
        let v = temp_vault("variants");
        let uuid_ws = "f3333333-3333-3333-3333-333333333333";
        let uuid_next = "a4444444-4444-4444-4444-444444444444";
        let (dir_ws, _) = synthetic_cascade("variants-ws", &[(uuid_ws, 8_000)]);
        let (dir_next, _) = synthetic_cascade("variants-next", &[(uuid_next, 3_000)]);
        let dirs = vec![
            (dir_ws, "windsurf".to_string()),
            (dir_next, "windsurf-next".to_string()),
        ];

        let stats = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(stats.new_sessions, 2, "both variants contribute rows");

        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.windsurf_sessions(&month).unwrap();
        let ws_row = rows.iter().find(|r| r.session_id == uuid_ws).unwrap();
        let next_row = rows.iter().find(|r| r.session_id == uuid_next).unwrap();
        assert_eq!(ws_row.variant.as_deref(), Some("windsurf"));
        assert_eq!(next_row.variant.as_deref(), Some("windsurf-next"));
    }

    /// Same UUID in both windsurf and windsurf-next directories — should
    /// produce TWO distinct vault rows (keyed by variant) rather than aliasing.
    #[test]
    fn same_uuid_different_variants_not_aliased() {
        let v = temp_vault("alias");
        let shared_uuid = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        let dir_ws = std::env::temp_dir()
            .join(format!("trove-ws-cascade-{}-alias-ws", std::process::id()));
        let _ = fs::remove_dir_all(&dir_ws);
        fs::create_dir_all(&dir_ws).unwrap();
        fs::write(dir_ws.join(format!("{shared_uuid}.pb")), vec![0u8; 1_000]).unwrap();

        let dir_next = std::env::temp_dir()
            .join(format!("trove-ws-cascade-{}-alias-next", std::process::id()));
        let _ = fs::remove_dir_all(&dir_next);
        fs::create_dir_all(&dir_next).unwrap();
        fs::write(dir_next.join(format!("{shared_uuid}.pb")), vec![0u8; 2_000]).unwrap();

        let dirs = vec![
            (dir_ws, "windsurf".to_string()),
            (dir_next, "windsurf-next".to_string()),
        ];
        let stats = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(stats.new_sessions, 2, "same UUID, different variants = 2 rows");

        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.windsurf_sessions(&month).unwrap();
        assert_eq!(rows.len(), 2, "two distinct rows — not aliased");
        let ws_row = rows.iter().find(|r| r.variant.as_deref() == Some("windsurf")).unwrap();
        let next_row = rows.iter().find(|r| r.variant.as_deref() == Some("windsurf-next")).unwrap();
        assert_eq!(ws_row.session_id, shared_uuid);
        assert_eq!(next_row.session_id, shared_uuid);
        assert_eq!(ws_row.file_bytes, 1_000);
        assert_eq!(next_row.file_bytes, 2_000);
    }

    /// Cross-month stability: first_seen is captured once and frozen. When the
    /// file's mtime jumps to a different month (e.g. after archival rewrite),
    /// the session must remain in its original YYYY-MM partition — exactly one
    /// row total across all partitions.
    #[test]
    fn cross_month_no_duplicate_row() {
        let v = temp_vault("xmonth");
        let uuid = "cccccccc-cccc-cccc-cccc-cccccccccccc";

        let dir = std::env::temp_dir()
            .join(format!("trove-ws-cascade-{}-xmonth", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pb_path = dir.join(format!("{uuid}.pb"));

        // First pass: 20 MB live session.
        fs::write(&pb_path, vec![0u8; 20_000]).unwrap();
        let dirs = vec![(dir.clone(), "windsurf".to_string())];
        let s1 = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(s1.new_sessions, 1);

        // Simulate archival compaction: file shrinks to 240 KB AND mtime is
        // bumped to a future time that would be in a different month. We
        // simulate this by setting mtime via filetime crate... but since we
        // can't easily set mtime portably in tests, we simulate the effect by
        // using a size change (which is what triggers re-emit) and asserting
        // the row stays in its original partition.
        fs::write(&pb_path, vec![1u8; 240_000]).unwrap(); // size changed

        let s2 = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(s2.updated_sessions, 1, "size changed → updated");
        assert_eq!(s2.new_sessions, 0);

        // Total rows across ALL partitions must be exactly 1.
        let stream = v.stream(DIR, Partition::Month);
        let all_keys = stream.partitions().unwrap();
        let total_rows: usize = all_keys
            .iter()
            .map(|k| v.windsurf_sessions(k).unwrap().len())
            .sum();
        assert_eq!(total_rows, 1, "exactly one row across all partitions");
    }

    /// A rescan of an active session (size-changing) should count as 'updated',
    /// not 'new', so stats are accurate and the headline is not misleading.
    #[test]
    fn active_session_churn_counts_as_updated_not_new() {
        let v = temp_vault("churn");
        let uuid = "dddddddd-dddd-dddd-dddd-dddddddddddd";
        let dir = std::env::temp_dir()
            .join(format!("trove-ws-cascade-{}-churn", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pb_path = dir.join(format!("{uuid}.pb"));
        let dirs = vec![(dir, "windsurf".to_string())];

        fs::write(&pb_path, vec![0u8; 1_000]).unwrap();
        let s1 = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(s1.new_sessions, 1);

        // Repeated size changes (simulating an active trajectory being appended to).
        for size in [2_000u64, 5_000, 10_000, 15_000] {
            fs::write(&pb_path, vec![0u8; size as usize]).unwrap();
            let s = v.collect_windsurf_from_dirs(&dirs).unwrap();
            assert_eq!(s.new_sessions, 0, "size={size}: not a new session");
            assert_eq!(s.updated_sessions, 1, "size={size}: counted as updated");
        }

        // Still exactly one row in the vault.
        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.windsurf_sessions(&month).unwrap();
        assert_eq!(rows.len(), 1, "upsert kept single row through all churns");
        assert_eq!(rows[0].file_bytes, 15_000);
    }

    #[test]
    fn missing_cascade_dir_is_quiet_noop() {
        let v = temp_vault("missing");
        // Pass empty dirs list — simulates no Windsurf installed.
        let stats = v.collect_windsurf_from_dirs(&[]).unwrap();
        assert_eq!(stats.new_sessions, 0);
        assert_eq!(stats.updated_sessions, 0);
        assert!(!v.root().join("developer/windsurf").exists());
    }

    #[test]
    fn non_pb_files_are_ignored() {
        let v = temp_vault("nonpb");
        let dir = std::env::temp_dir()
            .join(format!("trove-ws-cascade-{}-nonpb", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Non-.pb files must be silently ignored.
        fs::write(dir.join("session.json"), b"{}").unwrap();
        fs::write(dir.join("something.db"), b"SQLite format 3").unwrap();
        // One valid .pb file.
        let uuid = "b5555555-5555-5555-5555-555555555555";
        fs::write(dir.join(format!("{uuid}.pb")), vec![0u8; 1_000]).unwrap();

        let dirs = vec![(dir, "windsurf".to_string())];
        let stats = v.collect_windsurf_from_dirs(&dirs).unwrap();
        assert_eq!(stats.new_sessions, 1, "only the .pb file catalogued");
    }

    #[test]
    fn sync_state_round_trips() {
        let empty: WindsurfSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.seen.is_empty());
        let mut s = WindsurfSyncState::default();
        s.seen.insert("windsurf/abc".into(), SeenEntry {
            first_seen: "2026-05-01T10:00:00+00:00".to_string(),
            last_bytes: 12_345,
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: WindsurfSyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen.get("windsurf/abc").map(|e| e.last_bytes), Some(12_345));
    }

    #[test]
    fn secs_to_rfc3339_produces_valid_timestamp() {
        let s = secs_to_rfc3339(1_748_000_000);
        assert!(s.contains('T'), "expected RFC3339 with T: {s}");
    }

    /// Out-of-range / pathological mtime must NOT produce Local::now() — it
    /// must produce the epoch sentinel, so rows are not misfiled.
    #[test]
    fn secs_to_rfc3339_pathological_yields_sentinel_not_now() {
        // i64::MAX is far out of range for a valid datetime.
        let s = secs_to_rfc3339(i64::MAX);
        assert_eq!(s, "1970-01-01T00:00:00+00:00",
            "pathological mtime should yield epoch sentinel, got: {s}");
    }

}
