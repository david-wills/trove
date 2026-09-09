//! Google Drive for Desktop — file-tree index from the local mirror.
//!
//! Scans the Google Drive for Desktop local folder using the shared
//! [`crate::cloud_folder`] walker: lstat-only, never reads file contents,
//! records placeholder flag for online-only (stream-mode) files.
//!
//! Path probe: Google Drive for Desktop creates account-suffixed directories
//! under `~/Library/CloudStorage/` with the prefix `GoogleDrive-`. Each
//! account folder contains a `My Drive/` subdirectory (the user's personal
//! files) and optionally a `Shared drives/` subdirectory.  We index `My Drive/`
//! (and `Shared drives/` if present) for every account found.
//!
//! Stub files: Google Docs/Sheets/Slides/Forms are never full local files —
//! they appear as tiny JSON stubs (`.gdoc`, `.gsheet`, `.gslides`, `.gform`,
//! `.gmap`, `.gdraw`, `.gsite`, `.gjam`). In **stream mode** (the default)
//! these stubs ARE marked SF_DATALESS by Google Drive for Desktop even though
//! they are tiny local JSON files — empirically, 99 % of stubs on a real
//! install show `st_flags=0x40000060` (SF_DATALESS | SF_COMPRESSED | …).
//! They are therefore recorded with `placeholder: true, size: 0` by the
//! shared cloud_folder walker. Their extension (`.gdoc`, `.gsheet`, …) is
//! still captured, making them first-class metadata rows distinguishable from
//! binary placeholders by extension.
//!
//! Online-only placeholders (stream mode): real binary files that have not
//! been downloaded also have SF_DATALESS set (`0x40000000` in `st_flags`).
//! The shared cloud_folder walker detects both cases via `real_is_dataless`
//! and records them with `placeholder: true` without opening them.
//!
//! `TROVE_HOME` env var overrides `HOME` in tests.
//!
//! Brief: `docs/integrations/google-drive.md`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::DateTime;

use crate::cloud_folder;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const VAULT_PATH: &str = "files/google-drive";
const SCAN_INTERVAL_SECS: u64 = 900; // 15-minute periodic scan.

// ---------------------------------------------------------------------------
// Path probe

/// Resolve all Google Drive roots from the environment.
///
/// Returns a Vec of (`account`, `root`) pairs (one per account subfolder found
/// under `~/Library/CloudStorage/GoogleDrive-*`). The `root` is either the
/// `My Drive/` subfolder (if present) or the account folder itself.
///
/// Uses `TROVE_HOME` first (for tests), then falls back to `HOME`.
pub fn find_google_drive_roots() -> Vec<(String, PathBuf)> {
    let home = match home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    probe_google_drive_roots(&home)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("TROVE_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Walk `~/Library/CloudStorage/` for `GoogleDrive-*` entries and return
/// all usable roots (with account name).
fn probe_google_drive_roots(home: &Path) -> Vec<(String, PathBuf)> {
    let cloud = home.join("Library/CloudStorage");
    let entries = match std::fs::read_dir(&cloud) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut roots = Vec::new();
    let mut account_dirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_owned();
            if name.starts_with("GoogleDrive-") && path.is_dir() {
                let account = name["GoogleDrive-".len()..].to_owned();
                Some((account, path))
            } else {
                None
            }
        })
        .collect();

    // Sort for deterministic ordering across accounts.
    account_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    for (account, acct_dir) in account_dirs {
        // Primary: My Drive subfolder.
        let my_drive = acct_dir.join("My Drive");
        if is_accessible_dir(&my_drive) {
            roots.push((account.clone(), my_drive));
        }
        // Secondary: Shared drives subfolder (if present).
        let shared = acct_dir.join("Shared drives");
        if is_accessible_dir(&shared) {
            roots.push((format!("{account}/Shared drives"), shared));
        }
        // Fallback: account dir itself if no "My Drive" was found.
        if !is_accessible_dir(&acct_dir.join("My Drive")) && is_accessible_dir(&acct_dir) {
            roots.push((account, acct_dir));
        }
    }

    roots
}

fn is_accessible_dir(path: &Path) -> bool {
    path.is_dir() && std::fs::read_dir(path).is_ok()
}

// ---------------------------------------------------------------------------
// DEF hooks

fn def_collect(vault: &Vault, _now: DateTime<chrono::Local>) -> Result<CollectOutcome> {
    let roots = find_google_drive_roots();
    if roots.is_empty() {
        return Err(anyhow::anyhow!(
            "Google Drive for Desktop folder not found under ~/Library/CloudStorage/GoogleDrive-*"
        ));
    }

    let mut total_files = 0u64;
    let mut total_dirs = 0u64;
    let mut total_added = 0u64;
    let mut total_modified = 0u64;
    let mut total_removed = 0u64;
    let mut total_placeholders = 0u64;
    let mut any_baseline = false;

    for (account, root) in &roots {
        // Sanitize account name for use as a vault sub-path component.
        let safe_account = account.replace(['/', '@', ' '], "_");
        let vault_sub = format!("{VAULT_PATH}/{safe_account}");

        let stats = cloud_folder::scan_and_diff(
            vault,
            &vault_sub,
            root,
            cloud_folder::real_is_dataless,
        )?;

        total_files += stats.total_files;
        total_dirs += stats.total_dirs;
        total_added += stats.added;
        total_modified += stats.modified;
        total_removed += stats.removed;
        total_placeholders += stats.placeholders;
        if stats.baseline {
            any_baseline = true;
        }
    }

    if any_baseline && total_added == 0 && total_modified == 0 && total_removed == 0 {
        return Ok(CollectOutcome::note(format!(
            "google-drive baseline: indexed {} files, {} dirs ({} placeholders) across {} root(s)",
            total_files,
            total_dirs,
            total_placeholders,
            roots.len(),
        )));
    }

    Ok(CollectOutcome::note_if(
        total_added + total_modified + total_removed > 0,
        || {
            format!(
                "google-drive scan: {} files, {} dirs, +{} ~{} -{} ({} placeholders)",
                total_files,
                total_dirs,
                total_added,
                total_modified,
                total_removed,
                total_placeholders,
            )
        },
    ))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    let granted = !find_google_drive_roots().is_empty();
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(granted),
        required: false, // Full Disk Access grant Trove already holds covers this.
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    // Scan all account sub-paths and return the most recent event timestamp.
    let base = vault.root().join(VAULT_PATH);
    let account_dirs = std::fs::read_dir(&base).ok()?;

    let mut latest: Option<String> = None;

    for entry in account_dirs.flatten() {
        let acct_path = entry.path();
        if !acct_path.is_dir() {
            continue;
        }
        let acct_name = acct_path.file_name()?.to_str()?.to_owned();
        let vault_sub = format!("{VAULT_PATH}/{acct_name}");

        let stream = vault.stream(&format!("{vault_sub}/events"), Partition::Month);
        if let Ok(parts) = stream.partitions() {
            if let Some(part) = parts.last() {
                if let Ok(events) = stream.read::<cloud_folder::ChangeEvent>(part) {
                    if let Some(last) = events.last() {
                        match &latest {
                            None => latest = Some(last.ts.clone()),
                            Some(prev) if last.ts > *prev => latest = Some(last.ts.clone()),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    if latest.is_some() {
        return latest;
    }

    // Fallback: snapshot mtime from the first account dir found.
    if let Ok(mut dirs) = std::fs::read_dir(&base) {
        if let Some(Ok(entry)) = dirs.next() {
            let acct_name = entry.file_name().to_string_lossy().into_owned();
            return crate::registry::file_mtime(
                &vault.root().join(format!("{VAULT_PATH}/{acct_name}/snapshot.jsonl")),
            );
        }
    }

    None
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-drive",
        name: "Google Drive",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Indexes your Google Drive for Desktop folder — file names, paths, \
                      sizes, and modification times — by scanning the local mirror. \
                      Online-only files (stream mode) are noted as placeholders and \
                      are never downloaded. Google Docs/Sheets/Slides stubs are \
                      recorded by name and type.",
        domain: "files",
        vault_path: "files/google-drive/",
        toggleable: true,
        setup: &[
            "Install Google Drive for Desktop and sign in.",
            "Uses the Full Disk Access grant Trove already holds — no additional permission needed.",
        ],
        caveats: "Probes ~/Library/CloudStorage/GoogleDrive-<account>/ for all \
                  signed-in accounts. Online-only placeholder files (stream mode) \
                  are recorded with placeholder=true and are never downloaded. \
                  Google Docs/Sheets/Slides appear as .gdoc/.gsheet/.gslides stubs \
                  with their names; content export requires the Drive API (not yet wired).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(SCAN_INTERVAL_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trove-gdrive-home-{}-{}-{tag}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos() as u64
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_cloud_dir(home: &Path) -> PathBuf {
        let cloud = home.join("Library/CloudStorage");
        fs::create_dir_all(&cloud).unwrap();
        cloud
    }

    // -----------------------------------------------------------------------
    // Test 1: probe finds a single account's My Drive.

    #[test]
    fn probe_finds_single_account_my_drive() {
        let home = temp_home("single");
        let cloud = make_cloud_dir(&home);
        let my_drive = cloud.join("GoogleDrive-user@example.com/My Drive");
        fs::create_dir_all(&my_drive).unwrap();

        let roots = probe_google_drive_roots(&home);
        assert_eq!(roots.len(), 1, "one account root");
        assert_eq!(roots[0].0, "user@example.com");
        assert_eq!(roots[0].1, my_drive);

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 2: probe finds multiple accounts, sorts deterministically.

    #[test]
    fn probe_finds_multiple_accounts_sorted() {
        let home = temp_home("multi");
        let cloud = make_cloud_dir(&home);
        fs::create_dir_all(cloud.join("GoogleDrive-b@example.com/My Drive")).unwrap();
        fs::create_dir_all(cloud.join("GoogleDrive-a@example.com/My Drive")).unwrap();

        let roots = probe_google_drive_roots(&home);
        // Two My Drive roots, sorted by account name.
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].0, "a@example.com");
        assert_eq!(roots[1].0, "b@example.com");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 3: Shared drives sub-path is indexed as a secondary root.

    #[test]
    fn probe_includes_shared_drives() {
        let home = temp_home("shared");
        let cloud = make_cloud_dir(&home);
        let acct = cloud.join("GoogleDrive-user@example.com");
        fs::create_dir_all(acct.join("My Drive")).unwrap();
        fs::create_dir_all(acct.join("Shared drives/TeamDrive")).unwrap();

        let roots = probe_google_drive_roots(&home);
        // Should have two entries: My Drive + Shared drives.
        assert_eq!(roots.len(), 2);
        let has_shared = roots.iter().any(|(acct, _)| acct.contains("Shared drives"));
        assert!(has_shared, "Shared drives should be a secondary root");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 4: no Google Drive installed → empty vec, not a panic.

    #[test]
    fn probe_returns_empty_when_not_installed() {
        let home = temp_home("none");
        // No CloudStorage dir at all.
        let roots = probe_google_drive_roots(&home);
        assert!(roots.is_empty());

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 5: non-GoogleDrive directories under CloudStorage are ignored.

    #[test]
    fn probe_ignores_non_google_dirs() {
        let home = temp_home("other");
        let cloud = make_cloud_dir(&home);
        fs::create_dir_all(cloud.join("Dropbox")).unwrap();
        fs::create_dir_all(cloud.join("Box-user@example.com")).unwrap();

        let roots = probe_google_drive_roots(&home);
        assert!(roots.is_empty(), "non-GoogleDrive dirs must be ignored");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 6: scan + diff via cloud_folder — baseline + change detection.

    fn temp_vault(tag: &str) -> crate::vault::Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-gdrive-vault-{}-{}-{tag}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos() as u64
        ));
        let _ = fs::remove_dir_all(&dir);
        crate::vault::Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn scan_baseline_then_change() {
        let home = temp_home("scan");
        let cloud = make_cloud_dir(&home);
        let my_drive = cloud.join("GoogleDrive-test@example.com/My Drive");
        fs::create_dir_all(&my_drive).unwrap();

        // Seed with a real file and a Google Docs stub.
        fs::write(my_drive.join("report.pdf"), b"pdf content").unwrap();
        fs::write(
            my_drive.join("Plan.gdoc"),
            br#"{"doc_id":"abc123","email":"test@example.com"}"#,
        )
        .unwrap();

        let vault = temp_vault("scan");

        // Seam: treat .gdoc as dataless (mirrors real Drive stream-mode behavior
        // where stubs are SF_DATALESS even though they are tiny local JSON files).
        let gdoc_is_dataless = |p: &std::path::Path, _m: &std::fs::Metadata| {
            p.extension().is_some_and(|e| e == "gdoc" || e == "gsheet" || e == "gslides")
        };

        // Baseline scan.
        let stats = cloud_folder::scan_and_diff(
            &vault,
            "files/google-drive/test_example_com",
            &my_drive,
            gdoc_is_dataless,
        )
        .unwrap();
        assert!(stats.baseline);
        assert_eq!(stats.total_files, 2, "pdf + gdoc stub");
        assert_eq!(stats.added, 0, "baseline emits no events");
        assert_eq!(stats.placeholders, 1, "gdoc stub is a placeholder in stream mode");

        // Verify snapshot captured the .gdoc extension and placeholder=true.
        let snap: Vec<cloud_folder::FileRecord> = vault
            .read_snapshot("files/google-drive/test_example_com/snapshot.jsonl")
            .unwrap();
        let gdoc = snap.iter().find(|r| r.name == "Plan.gdoc").expect(".gdoc in snapshot");
        assert_eq!(gdoc.ext.as_deref(), Some("gdoc"), ".gdoc extension captured");
        assert!(gdoc.placeholder, "stream-mode gdoc stub is flagged placeholder=true");
        assert_eq!(gdoc.size, 0, "placeholder size is 0");

        // Add a new .gsheet stub → emitted as "added" event.
        fs::write(my_drive.join("new.gsheet"), br#"{"doc_id":"xyz789","email":"test@example.com"}"#).unwrap();
        let stats2 = cloud_folder::scan_and_diff(
            &vault,
            "files/google-drive/test_example_com",
            &my_drive,
            gdoc_is_dataless,
        )
        .unwrap();
        assert!(!stats2.baseline);
        assert_eq!(stats2.added, 1);
        assert_eq!(stats2.modified, 0);
        assert_eq!(stats2.removed, 0);

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 7: placeholder files (online-only stream-mode) via seam injection.

    #[test]
    fn placeholder_stream_mode_file() {
        let home = temp_home("ph");
        let cloud = make_cloud_dir(&home);
        let my_drive = cloud.join("GoogleDrive-test@example.com/My Drive");
        fs::create_dir_all(&my_drive).unwrap();

        // A big binary that "hasn't been downloaded" — injected via seam.
        fs::write(my_drive.join("video.mp4"), b"placeholder").unwrap();

        let vault = temp_vault("ph");

        // Seam: treat video.mp4 as dataless (stream-mode placeholder).
        let seam = |p: &Path, _m: &std::fs::Metadata| {
            p.file_name().is_some_and(|n| n == "video.mp4")
        };

        cloud_folder::scan_and_diff(
            &vault,
            "files/google-drive/ph_test",
            &my_drive,
            seam,
        )
        .unwrap();

        let snap: Vec<cloud_folder::FileRecord> = vault
            .read_snapshot("files/google-drive/ph_test/snapshot.jsonl")
            .unwrap();
        let ph = snap.iter().find(|r| r.name == "video.mp4").expect("placeholder present");
        assert!(ph.placeholder, "stream-mode file must be flagged placeholder");
        assert_eq!(ph.size, 0, "placeholder size must be 0");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 8: account name sanitization for vault sub-paths.

    #[test]
    fn account_name_sanitized_for_vault_path() {
        // Verify our sanitization logic: '@', ' ', '/' → '_'
        let account = "user@example.com";
        let safe = account.replace(['/', '@', ' '], "_");
        assert_eq!(safe, "user_example.com");

        let account2 = "user@example.com/Shared drives";
        let safe2 = account2.replace(['/', '@', ' '], "_");
        assert_eq!(safe2, "user_example.com_Shared_drives");
    }
}
