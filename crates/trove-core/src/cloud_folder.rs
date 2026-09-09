//! Reusable cloud-folder walker: scan a local mirror directory tree, diff
//! against a previous snapshot, emit change events. Shared by Dropbox,
//! iCloud Drive, OneDrive, Google Drive, and Box — same algorithm, different
//! root paths.
//!
//! **Never reads file contents** — only `lstat` metadata (size, timestamps,
//! flags). Online-only placeholder files are identified by the `is_dataless`
//! seam (injectable for tests) and recorded with `placeholder: true` without
//! descending into them or reading bytes from them.
//!
//! Vault layout per integration:
//! - `<vault_path>/snapshot.jsonl` — atomically rewritten full file index.
//! - `<vault_path>/events/YYYY-MM.jsonl` — month-partitioned change log.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Public data types

/// One file or directory entry in the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Path relative to the cloud folder root, using `/` separators.
    pub path: String,
    /// File name (last component).
    pub name: String,
    /// File extension without the dot, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext: Option<String>,
    /// File size in bytes (`st_size`). Zero for directories and placeholders.
    pub size: u64,
    /// RFC3339 creation time.
    pub created: String,
    /// RFC3339 modification time.
    pub modified: String,
    /// True when this entry is a directory.
    pub is_dir: bool,
    /// True when the file is an online-only placeholder (not downloaded).
    /// Omitted from JSON when false to keep rows compact.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub placeholder: bool,
}

/// One change event appended to the month-partitioned event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeEvent {
    /// RFC3339 time of the scan that observed this change.
    pub ts: String,
    /// "added" | "modified" | "removed"
    pub kind: String,
    /// Path relative to root, `/`-separated.
    pub path: String,
    /// File name.
    pub name: String,
    /// Extension without dot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext: Option<String>,
    /// File size in bytes.
    pub size: u64,
    /// True if directory.
    pub is_dir: bool,
    /// True if online-only placeholder.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub placeholder: bool,
    /// Stable dedupe key: SHA-256 hex of `"<rel_path>|<kind>|<mtime_rfc3339>"`.
    pub guid: String,
}

/// Counts from one scan+diff pass, for logging.
#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub total_files: u64,
    pub total_dirs: u64,
    pub added: u64,
    pub modified: u64,
    pub removed: u64,
    pub placeholders: u64,
    /// True when this was the first-ever scan (no prior snapshot file): the
    /// snapshot is written but NO change events are emitted (silent baseline).
    pub baseline: bool,
}

// ---------------------------------------------------------------------------
// Core scan + diff function

/// Scan `root`, diff against the previous snapshot in the vault, append
/// change events, and write the new snapshot. Returns statistics.
///
/// `is_dataless(path, metadata)` is called for every entry; return `true` to
/// mark it as a placeholder. The seam is injectable so tests can exercise the
/// placeholder path without needing real macOS `SF_DATALESS` files.
///
/// # Errors
/// Returns an error if the vault cannot be read or written, or if the root
/// directory cannot be listed. Individual entries that cannot be `lstat`-ed
/// are silently skipped (drive-managed moves during a scan).
pub fn scan_and_diff<F>(
    vault: &Vault,
    vault_path: &str,
    root: &Path,
    is_dataless: F,
) -> Result<ScanStats>
where
    F: Fn(&Path, &std::fs::Metadata) -> bool,
{
    let now_ts = DateTime::<Local>::from(SystemTime::now()).to_rfc3339();

    // 1. Walk the directory tree (lstat — never follows symlinks).
    let mut new_records: Vec<FileRecord> = Vec::new();
    walk(root, root, &is_dataless, &mut new_records)?;

    // Sort for stable snapshot output and deterministic diff ordering.
    new_records.sort_by(|a, b| a.path.cmp(&b.path));

    let snapshot_rel = format!("{vault_path}/snapshot.jsonl");

    // 2. First-ever scan = silent baseline: write the snapshot, emit NO events.
    //    Keyed on the snapshot FILE existing — not on `old` being empty (an
    //    empty old snapshot after everything was deleted is a real diff, not a
    //    baseline). Mirrors books.rs / podcasts.rs. Without this, a real
    //    Dropbox would flood events/ with thousands of spurious "added" rows on
    //    first sync — and every inheriting watcher (iCloud/OneDrive/Drive/Box)
    //    would do the same.
    let baseline = !vault.resolve(&snapshot_rel)?.exists();
    if baseline {
        vault.write_snapshot(&snapshot_rel, &new_records)?;
        let mut stats = count_records(&new_records);
        stats.baseline = true;
        return Ok(stats);
    }

    // 3. Load the previous snapshot and diff.
    let old_records: Vec<FileRecord> = vault.read_snapshot(&snapshot_rel)?;
    let (events, stats) = diff(&old_records, &new_records, &now_ts);

    // 4. Append events (month-partitioned).
    if !events.is_empty() {
        let events_dir = format!("{vault_path}/events");
        vault.stream(&events_dir, Partition::Month).append(&events, |e| &e.ts)?;
    }

    // 5. Atomically rewrite the snapshot.
    vault.write_snapshot(&snapshot_rel, &new_records)?;

    Ok(stats)
}

/// Tally file/dir/placeholder counts from a record set (used for the baseline
/// pass, which records totals but emits no change events).
fn count_records(records: &[FileRecord]) -> ScanStats {
    let mut stats = ScanStats::default();
    for rec in records {
        if rec.is_dir {
            stats.total_dirs += 1;
        } else {
            stats.total_files += 1;
        }
        if rec.placeholder {
            stats.placeholders += 1;
        }
    }
    stats
}

// ---------------------------------------------------------------------------
// Walk: lstat-based, placeholder-aware, never reads file contents.

fn walk<F>(
    root: &Path,
    dir: &Path,
    is_dataless: &F,
    out: &mut Vec<FileRecord>,
) -> Result<()>
where
    F: Fn(&Path, &std::fs::Metadata) -> bool,
{
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()), // Unreadable dir (permission-gated, etc.) — skip silently.
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // lstat — never follow symlinks (avoids triggering Dropbox downloads
        // for symlink-based placeholder schemes, and is just correct).
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue, // Vanished between readdir and lstat — skip.
        };

        // Skip symlinks entirely (they are not real file/dir entries).
        if meta.file_type().is_symlink() {
            continue;
        }

        let dataless = is_dataless(&path, &meta);
        let rec = make_record(root, &path, &meta, dataless)?;

        // Recurse into real directories, but never into dataless directories
        // (which would trigger a bulk download of the entire subtree).
        let is_actual_dir = meta.is_dir() && !dataless;
        out.push(rec);
        if is_actual_dir {
            walk(root, &path, is_dataless, out)?;
        }
    }
    Ok(())
}

fn make_record(
    root: &Path,
    path: &Path,
    meta: &std::fs::Metadata,
    dataless: bool,
) -> Result<FileRecord> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("path {} is not under root {}", path.display(), root.display()))?;
    // Use forward slashes regardless of OS, for stable vault content.
    let rel_str = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let ext = if meta.is_dir() {
        None
    } else {
        path.extension().map(|e| e.to_string_lossy().to_lowercase())
    };

    // ctime is the inode-change time on macOS, not creation time.
    // Use birthtime (st_birthtime) if available, else fall back to mtime.
    #[cfg(target_os = "macos")]
    let created = {
        use std::os::darwin::fs::MetadataExt as DarwinExt;
        let btime_secs = meta.st_birthtime();
        let btime_nsecs = meta.st_birthtime_nsec();
        // birthtime = 0 means unknown; fall back to mtime.
        if btime_secs > 0 {
            DateTime::<Local>::from(
                SystemTime::UNIX_EPOCH
                    + std::time::Duration::new(btime_secs as u64, btime_nsecs as u32),
            )
            .to_rfc3339()
        } else {
            meta.modified()
                .map(|t| DateTime::<Local>::from(t).to_rfc3339())
                .unwrap_or_else(|_| "1970-01-01T00:00:00+00:00".to_string())
        }
    };
    #[cfg(not(target_os = "macos"))]
    let created = meta
        .modified()
        .map(|t| DateTime::<Local>::from(t).to_rfc3339())
        .unwrap_or_else(|_| "1970-01-01T00:00:00+00:00".to_string());

    let modified = meta
        .modified()
        .map(|t| DateTime::<Local>::from(t).to_rfc3339())
        .unwrap_or_else(|_| "1970-01-01T00:00:00+00:00".to_string());

    // Placeholders report 0 size (their reported st_size is the stub size,
    // not the actual content size, and we never need it).
    let size = if dataless || meta.is_dir() { 0 } else { meta.len() };

    Ok(FileRecord {
        path: rel_str,
        name,
        ext,
        size,
        created,
        modified,
        is_dir: meta.is_dir(),
        placeholder: dataless,
    })
}

// ---------------------------------------------------------------------------
// Diff: compare new vs old by path key.

fn diff(
    old: &[FileRecord],
    new: &[FileRecord],
    ts: &str,
) -> (Vec<ChangeEvent>, ScanStats) {
    let old_map: BTreeMap<&str, &FileRecord> = old.iter().map(|r| (r.path.as_str(), r)).collect();
    let new_map: BTreeMap<&str, &FileRecord> = new.iter().map(|r| (r.path.as_str(), r)).collect();

    let mut events = Vec::new();
    let mut stats = ScanStats::default();

    for rec in new {
        if rec.is_dir {
            stats.total_dirs += 1;
        } else {
            stats.total_files += 1;
        }
        if rec.placeholder {
            stats.placeholders += 1;
        }

        match old_map.get(rec.path.as_str()) {
            None => {
                // New entry.
                stats.added += 1;
                events.push(make_event(ts, "added", rec));
            }
            Some(prev) => {
                // Changed if mtime or size differs.
                if prev.modified != rec.modified || prev.size != rec.size {
                    stats.modified += 1;
                    events.push(make_event(ts, "modified", rec));
                }
                // Otherwise: no event (unchanged).
            }
        }
    }

    // Removals: entries in old but not in new.
    for rec in old {
        if !new_map.contains_key(rec.path.as_str()) {
            stats.removed += 1;
            events.push(make_event(ts, "removed", rec));
        }
    }

    (events, stats)
}

fn make_event(ts: &str, kind: &str, rec: &FileRecord) -> ChangeEvent {
    let guid = stable_guid(&rec.path, kind, &rec.modified, rec.size);
    ChangeEvent {
        ts: ts.to_string(),
        kind: kind.to_string(),
        path: rec.path.clone(),
        name: rec.name.clone(),
        ext: rec.ext.clone(),
        size: rec.size,
        is_dir: rec.is_dir,
        placeholder: rec.placeholder,
        guid,
    }
}

/// Stable dedupe key: SHA-256 hex of `"<path>|<kind>|<mtime>|<size>"`.
///
/// `size` is hashed alongside mtime so a change that keeps the same
/// mtime-second but a different size (or an add→remove→re-add at the same
/// mtime/size) does not collide with the prior event's guid. We deliberately
/// do NOT hash the scan `ts` — that would make a re-emitted identical change
/// produce a different guid and defeat dedupe. A same-mtime + same-size +
/// different-content change is genuinely indistinguishable without reading
/// content, so collapsing those is acceptable.
fn stable_guid(path: &str, kind: &str, mtime: &str, size: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(b"|");
    hasher.update(kind.as_bytes());
    hasher.update(b"|");
    hasher.update(mtime.as_bytes());
    hasher.update(b"|");
    hasher.update(size.to_le_bytes());
    let bytes = hasher.finalize();
    // hex-encode: first 16 bytes (128 bits) is more than enough for dedup.
    bytes[..16].iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// macOS dataless detection (production).

/// Returns `true` when `path` is an online-only placeholder file per Apple's
/// File Provider framework. Uses the `SF_DATALESS` `st_flags` bit (Apple TN3150).
///
/// `std::fs::symlink_metadata` (`lstat`) is used by the caller, so `meta` is
/// already from `lstat`. We just read the `st_flags` field from it via
/// `std::os::darwin::fs::MetadataExt::st_flags`.
#[cfg(target_os = "macos")]
pub fn real_is_dataless(_path: &Path, meta: &std::fs::Metadata) -> bool {
    use std::os::darwin::fs::MetadataExt;
    // SF_DATALESS = 0x40000000 per Apple TN3150 / <sys/stat.h>.
    const SF_DATALESS: u32 = 0x4000_0000;
    (meta.st_flags() & SF_DATALESS) != 0
}

/// Non-macOS stub: no files are ever dataless.
#[cfg(not(target_os = "macos"))]
pub fn real_is_dataless(_path: &Path, _meta: &std::fs::Metadata) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-cf-{}-{}-{tag}", std::process::id(), timestamp_suffix()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-cf-root-{}-{}-{tag}", std::process::id(), timestamp_suffix()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn timestamp_suffix() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos() as u64
    }

    fn never_dataless(_: &Path, _: &std::fs::Metadata) -> bool {
        false
    }

    // -----------------------------------------------------------------------
    // Test 1: snapshot — correct FileRecord count and fields.

    #[test]
    fn snapshot_correct_records() {
        let root = temp_dir("snap");
        let vault = temp_vault("snap");

        // Create: root/a.txt, root/sub/b.md
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/b.md"), b"world").unwrap();

        let stats = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert_eq!(stats.total_files, 2, "two files");
        assert_eq!(stats.total_dirs, 1, "one subdir");
        // First scan is a silent baseline: snapshot written, NO change events.
        assert!(stats.baseline, "first scan is a baseline");
        assert_eq!(stats.added, 0, "baseline emits no added events");
        assert_eq!(stats.modified, 0);
        assert_eq!(stats.removed, 0);
        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        assert!(parts.is_empty(), "baseline writes no event partitions");

        // Read back snapshot — written even though no events were emitted.
        let snap: Vec<FileRecord> =
            vault.read_snapshot("files/test/snapshot.jsonl").unwrap();
        assert_eq!(snap.len(), 3);

        // Find a.txt record.
        let file_rec = snap.iter().find(|r| r.name == "a.txt").expect("a.txt present");
        assert_eq!(file_rec.path, "a.txt");
        assert_eq!(file_rec.ext.as_deref(), Some("txt"));
        assert_eq!(file_rec.size, 5);
        assert!(!file_rec.is_dir);
        assert!(!file_rec.placeholder);

        // Find sub/ record.
        let dir_rec = snap.iter().find(|r| r.name == "sub").expect("sub present");
        assert!(dir_rec.is_dir);
        assert_eq!(dir_rec.size, 0);

        // Find sub/b.md record.
        let nested = snap.iter().find(|r| r.name == "b.md").expect("b.md present");
        assert_eq!(nested.path, "sub/b.md");
        assert_eq!(nested.ext.as_deref(), Some("md"));

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 2: diff added.

    #[test]
    fn diff_added() {
        let root = temp_dir("add");
        let vault = temp_vault("add");

        fs::write(root.join("existing.txt"), b"x").unwrap();
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();

        // Add a new file.
        fs::write(root.join("new.txt"), b"new").unwrap();
        let stats = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert_eq!(stats.added, 1);
        assert_eq!(stats.modified, 0);
        assert_eq!(stats.removed, 0);

        // Verify the event landed. First scan = silent baseline (0 events);
        // only the second scan's "added" for new.txt is logged.
        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        assert!(!parts.is_empty(), "events written");
        let events: Vec<ChangeEvent> =
            vault.stream("files/test/events", Partition::Month).read(&parts[0]).unwrap();
        let added_events: Vec<_> = events.iter().filter(|e| e.kind == "added").collect();
        assert_eq!(added_events.len(), 1, "only new.txt, not the baselined existing.txt");
        assert!(added_events.iter().any(|e| e.name == "new.txt"), "new.txt added event");

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 3: diff modified.

    #[test]
    fn diff_modified() {
        let root = temp_dir("mod");
        let vault = temp_vault("mod");

        let file = root.join("file.txt");
        fs::write(&file, b"v1").unwrap();
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();

        // Modify: write new content (changes size → triggers modified).
        fs::write(&file, b"v2-longer").unwrap();
        let stats = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert_eq!(stats.modified, 1, "one modified");
        assert_eq!(stats.added, 0);
        assert_eq!(stats.removed, 0);

        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        let events: Vec<ChangeEvent> =
            vault.stream("files/test/events", Partition::Month).read(&parts[0]).unwrap();
        let modified: Vec<_> = events.iter().filter(|e| e.kind == "modified").collect();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].name, "file.txt");

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 4: diff removed.

    #[test]
    fn diff_removed() {
        let root = temp_dir("rem");
        let vault = temp_vault("rem");

        let file = root.join("doomed.txt");
        fs::write(&file, b"bye").unwrap();
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();

        fs::remove_file(&file).unwrap();
        let stats = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.added, 0);
        assert_eq!(stats.modified, 0);

        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        let events: Vec<ChangeEvent> =
            vault.stream("files/test/events", Partition::Month).read(&parts[0]).unwrap();
        let removed: Vec<_> = events.iter().filter(|e| e.kind == "removed").collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "doomed.txt");

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 5: placeholder file — recorded with placeholder=true, never opened.

    #[test]
    fn placeholder_file_recorded_not_opened() {
        let root = temp_dir("ph");
        let vault = temp_vault("ph");

        // Write a file with no read perms to simulate something we must never open.
        let secret = root.join("online-only.docx");
        fs::write(&secret, b"secret").unwrap();

        // Seam: treat the target file as dataless.
        let seam = |p: &Path, _m: &std::fs::Metadata| p.file_name().is_some_and(|n| n == "online-only.docx");

        scan_and_diff(&vault, "files/test", &root, seam).unwrap();

        let snap: Vec<FileRecord> = vault.read_snapshot("files/test/snapshot.jsonl").unwrap();
        let ph = snap.iter().find(|r| r.name == "online-only.docx").expect("placeholder present");
        assert!(ph.placeholder);
        // Size must be 0 (we do not read content — we set size to 0 for placeholders).
        assert_eq!(ph.size, 0);

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 6: dataless directory — recorded but NOT descended into.

    #[test]
    fn dataless_dir_not_descended() {
        let root = temp_dir("phdir");
        let vault = temp_vault("phdir");

        // Create a subdir with a file inside.
        let subdir = root.join("cloud-dir");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("inside.txt"), b"hidden").unwrap();

        // Seam: the subdir itself is dataless — do not recurse.
        let seam = |p: &Path, m: &std::fs::Metadata| m.is_dir() && p.file_name().is_some_and(|n| n == "cloud-dir");

        scan_and_diff(&vault, "files/test", &root, seam).unwrap();

        let snap: Vec<FileRecord> = vault.read_snapshot("files/test/snapshot.jsonl").unwrap();
        // The dir itself is recorded…
        assert!(snap.iter().any(|r| r.name == "cloud-dir" && r.placeholder));
        // …but inside.txt must NOT appear (we never descended).
        assert!(!snap.iter().any(|r| r.name == "inside.txt"), "inner file must not be indexed");

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 7: guid stability — same file unchanged → same guid on rescan.

    #[test]
    fn guid_stability() {
        let root = temp_dir("guid");
        let vault = temp_vault("guid");

        // First scan = silent baseline (no events). Add a file, then scan to
        // produce a real "added" event carrying a guid.
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        fs::write(root.join("stable.txt"), b"unchanged").unwrap();
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        let ev1: Vec<ChangeEvent> =
            vault.stream("files/test/events", Partition::Month).read(&parts[0]).unwrap();
        let g1 = ev1.iter().find(|e| e.name == "stable.txt").map(|e| e.guid.clone()).unwrap();

        // Rescan (no file changes) — no new events, but the guid in the previous
        // event is stable: re-derive it from the same inputs and compare.
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        let snap: Vec<FileRecord> = vault.read_snapshot("files/test/snapshot.jsonl").unwrap();
        let rec = snap.iter().find(|r| r.name == "stable.txt").unwrap();
        let g2 = stable_guid(&rec.path, "added", &rec.modified, rec.size);
        assert_eq!(g1, g2, "guid must be stable across scans");

        // Distinct inputs → distinct guid: size is part of the key.
        let g_diff_size = stable_guid(&rec.path, "added", &rec.modified, rec.size + 1);
        assert_ne!(g1, g_diff_size, "guid must change when size differs");

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 8: no double-emit — unchanged rescan → 0 events.

    #[test]
    fn no_double_emit() {
        let root = temp_dir("nodup");
        let vault = temp_vault("nodup");

        // Baseline (no events), then add a file → exactly one "added".
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        fs::write(root.join("once.txt"), b"hello").unwrap();
        let s1 = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert_eq!(s1.added, 1);
        assert!(!s1.baseline, "second scan is not a baseline");

        // Third scan: file unchanged → no new events.
        let s2 = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert_eq!(s2.added, 0);
        assert_eq!(s2.modified, 0);
        assert_eq!(s2.removed, 0);

        // Confirm only 1 event total in the stream.
        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        let events: Vec<ChangeEvent> =
            vault.stream("files/test/events", Partition::Month).read(&parts[0]).unwrap();
        assert_eq!(events.len(), 1, "no duplicate events on unchanged rescan");

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 9: empty root — 0 records, no crash.

    #[test]
    fn empty_root_no_crash() {
        let root = temp_dir("empty");
        let vault = temp_vault("empty");

        let stats = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert_eq!(stats.total_files, 0);
        assert_eq!(stats.total_dirs, 0);
        assert_eq!(stats.added, 0);
        assert!(stats.baseline, "first scan of an empty root is still a baseline");

        let snap: Vec<FileRecord> = vault.read_snapshot("files/test/snapshot.jsonl").unwrap();
        assert!(snap.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 10: serde back-compat — FileRecord with no `placeholder` field deserializes.

    #[test]
    fn serde_back_compat_missing_placeholder() {
        let json = r#"{"path":"a.txt","name":"a.txt","size":5,"created":"2026-01-01T00:00:00+00:00","modified":"2026-01-01T00:00:00+00:00","is_dir":false}"#;
        let rec: FileRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.path, "a.txt");
        assert!(!rec.placeholder, "missing placeholder defaults to false");
        assert!(rec.ext.is_none(), "missing ext defaults to None");

        // Also: old record with placeholder:false must NOT serialize the field.
        let rec_false = FileRecord {
            path: "b.txt".into(),
            name: "b.txt".into(),
            ext: None,
            size: 0,
            created: "2026-01-01T00:00:00+00:00".into(),
            modified: "2026-01-01T00:00:00+00:00".into(),
            is_dir: false,
            placeholder: false,
        };
        let out = serde_json::to_string(&rec_false).unwrap();
        assert!(!out.contains("placeholder"), "false placeholder must be omitted");

        // placeholder:true IS serialized.
        let rec_true = FileRecord { placeholder: true, ..rec_false };
        let out2 = serde_json::to_string(&rec_true).unwrap();
        assert!(out2.contains("\"placeholder\":true"));
    }

    // -----------------------------------------------------------------------
    // Test 11: first scan = silent baseline (0 events), then a real change
    // emits exactly one event. Guards against the first-scan event flood that a
    // real cloud folder (thousands of files) would otherwise produce on first
    // sync — and which every inheriting watcher would inherit.

    #[test]
    fn first_scan_is_silent_baseline() {
        let root = temp_dir("baseline");
        let vault = temp_vault("baseline");

        // Pre-seed the tree, then scan: baseline writes the snapshot, no events.
        fs::write(root.join("a.txt"), b"one").unwrap();
        fs::write(root.join("b.txt"), b"two").unwrap();
        let s1 = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert!(s1.baseline, "first-ever scan is a baseline");
        assert_eq!(s1.added, 0);
        assert_eq!(s1.modified, 0);
        assert_eq!(s1.removed, 0);
        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        assert!(parts.is_empty(), "baseline emits zero events");
        // …but the snapshot IS written, capturing both files.
        let snap: Vec<FileRecord> = vault.read_snapshot("files/test/snapshot.jsonl").unwrap();
        assert_eq!(snap.len(), 2);

        // Now a real change: add one file. Second scan is NOT a baseline and
        // emits exactly one "added".
        fs::write(root.join("c.txt"), b"three").unwrap();
        let s2 = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert!(!s2.baseline, "second scan is not a baseline");
        assert_eq!(s2.added, 1, "exactly one added for the new file");
        let parts = vault.stream("files/test/events", Partition::Month).partitions().unwrap();
        let events: Vec<ChangeEvent> =
            vault.stream("files/test/events", Partition::Month).read(&parts[0]).unwrap();
        assert_eq!(events.len(), 1, "only the post-baseline change is logged");
        assert_eq!(events[0].kind, "added");
        assert_eq!(events[0].name, "c.txt");

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 12: an empty old snapshot (everything deleted) is a real diff, NOT
    // a baseline — the snapshot file already exists, so removals are emitted.

    #[test]
    fn empty_old_snapshot_is_not_a_baseline() {
        let root = temp_dir("emptyold");
        let vault = temp_vault("emptyold");

        // Baseline with one file.
        fs::write(root.join("gone.txt"), b"x").unwrap();
        scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();

        // Delete it; second scan diffs against the existing snapshot (which now
        // becomes empty) and emits a "removed".
        fs::remove_file(root.join("gone.txt")).unwrap();
        let s2 = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert!(!s2.baseline, "deleting everything is a diff, not a baseline");
        assert_eq!(s2.removed, 1);

        // Third scan against the now-empty snapshot file: still not a baseline,
        // no spurious events.
        let s3 = scan_and_diff(&vault, "files/test", &root, never_dataless).unwrap();
        assert!(!s3.baseline, "empty snapshot file is not a baseline");
        assert_eq!(s3.added, 0);
        assert_eq!(s3.removed, 0);

        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Test 13: production `real_is_dataless` — a normal temp file is not
    // dataless. Exercises the real `st_flags()` read and guards the
    // `std::os::darwin` API from silently breaking on macOS toolchain bumps.

    #[cfg(target_os = "macos")]
    #[test]
    fn real_is_dataless_false_for_normal_file() {
        let root = temp_dir("realflag");
        let file = root.join("normal.txt");
        fs::write(&file, b"materialized content").unwrap();

        let meta = std::fs::symlink_metadata(&file).unwrap();
        assert!(
            !real_is_dataless(&file, &meta),
            "a normal on-disk file must not be flagged dataless"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
