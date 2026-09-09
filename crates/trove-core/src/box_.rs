//! Box cloud storage — file-tree index from the local Box Drive folder.
//!
//! Scans the Box Drive local mirror (installed by the Box Drive desktop app)
//! using the shared [`crate::cloud_folder`] walker: lstat-only, never reads
//! file contents, records placeholder flag for online-only files.
//!
//! Path probe (priority order):
//! 1. `~/Library/CloudStorage/Box-Box` — the actual File Provider directory
//!    created by Box Drive on macOS 12.5+ (provider = "Box", domain display
//!    name = "Box"; Apple formats it as `<provider>-<domain>`, yielding
//!    "Box-Box"). This is the primary path confirmed by Apple Community threads
//!    and university IT documentation.
//! 2. `~/Library/CloudStorage/Box` — bare "Box" entry, kept for forward
//!    compatibility with potential future Box Drive versions.
//! 3. Any other `~/Library/CloudStorage/Box*` directory — prefers the
//!    canonical names above; alphabetical among the rest, skipping names that
//!    look like timestamped backups or "Old" variants.
//! 4. `~/Box`  (legacy pre-File-Provider path used by Box Drive < 2.21)
//!
//! `TROVE_HOME` env var overrides `HOME` in tests so the probe runs in a
//! controlled temp dir.
//!
//! Brief: `docs/integrations/box.md`.
//! Domain: `files/` (raw-only — no shared contract).

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::DateTime;

use crate::cloud_folder;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const VAULT_PATH: &str = "files/box";
const SCAN_INTERVAL_SECS: u64 = 3_600; // Hourly scan (Box Drive isn't a first-tier daily driver).

// ---------------------------------------------------------------------------
// Path probe

/// Resolve the Box Drive root from the environment. Checks `TROVE_HOME` first
/// (for tests), then falls back to `HOME`. Returns `None` if no Box Drive
/// folder is detected.
pub fn find_box_root() -> Option<PathBuf> {
    let home = home_dir()?;
    probe_box_root(&home)
}

/// `HOME` override for tests via `TROVE_HOME`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("TROVE_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Walk the priority list until a readable Box Drive directory is found.
fn probe_box_root(home: &Path) -> Option<PathBuf> {
    let cloud = home.join("Library/CloudStorage");

    // 1. Primary File Provider path: "Box-Box" (provider "Box" + domain display
    //    name "Box"). Apple's File Provider framework formats dirs as
    //    `<provider>-<domain>`, so Box Drive yields "Box-Box". This is confirmed
    //    by Apple Community threads and university IT docs ("Can't find
    //    ~/Library/CloudStorage/Box-Box").
    let box_box = cloud.join("Box-Box");
    if is_accessible_dir(&box_box) {
        return Some(box_box);
    }

    // 2. Bare "Box" — kept for forward compatibility with potential future
    //    Box Drive versions that might use the un-suffixed form.
    let exact = cloud.join("Box");
    if is_accessible_dir(&exact) {
        return Some(exact);
    }

    // 3. Any other `Box*` entry in CloudStorage. Prefers canonical names (already
    //    checked above); falls back alphabetically among the rest, excluding
    //    entries that look like timestamped backups or "Old" variants.
    if let Some(matched) = best_box_glob(&cloud) {
        return Some(matched);
    }

    // 4. Legacy `~/Box` (Box Drive < 2.21, pre-File-Provider).
    let legacy = home.join("Box");
    if is_accessible_dir(&legacy) {
        return Some(legacy);
    }

    None
}

/// Scan `~/Library/CloudStorage/` for entries whose name starts with "Box"
/// and pick the best one (excluding known noise patterns).
///
/// Canonical names ("Box-Box", "Box") are already probed by the caller before
/// this is reached, so they will not appear here. Among the remainder, prefer
/// entries that sort alphabetically earlier, but never a symlink (use
/// `symlink_metadata` so symlinked "Box*" dirs are excluded — the vault walk
/// itself is already lstat-only).
fn best_box_glob(cloud_storage: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(cloud_storage).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Exclude canonical names (already probed directly by caller).
            if name == "Box-Box" || name == "Box" {
                return false;
            }
            name.starts_with("Box")
                // Use symlink_metadata so symlinked dirs are excluded.
                && p.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false)
                && !name.contains("Old")
                && !looks_like_timestamped_backup(name)
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }
    candidates.sort();
    candidates.into_iter().find(|p| is_accessible_dir(p))
}

/// Heuristic: does the directory name look like a timestamped backup?
/// Matches patterns like "Box (2024-01-01)" or "Box (20240101)".
fn looks_like_timestamped_backup(name: &str) -> bool {
    if let (Some(open), Some(close)) = (name.find('('), name.rfind(')')) {
        if open < close {
            let inner = &name[open + 1..close];
            return inner.chars().filter(|c| c.is_ascii_digit()).count() >= 6;
        }
    }
    false
}

fn is_accessible_dir(path: &Path) -> bool {
    path.is_dir() && std::fs::read_dir(path).is_ok()
}

// ---------------------------------------------------------------------------
// DEF hooks

fn def_collect(vault: &Vault, _now: DateTime<chrono::Local>) -> Result<CollectOutcome> {
    let root = find_box_root()
        .ok_or_else(|| anyhow::anyhow!("Box Drive folder not found — install Box Drive and let it sync at least one file"))?;
    let stats = cloud_folder::scan_and_diff(
        vault,
        VAULT_PATH,
        &root,
        cloud_folder::real_is_dataless,
    )?;
    if stats.baseline {
        return Ok(CollectOutcome::note(format!(
            "box baseline: indexed {} files, {} dirs ({} placeholders)",
            stats.total_files, stats.total_dirs, stats.placeholders,
        )));
    }
    Ok(CollectOutcome::note_if(
        stats.added + stats.modified + stats.removed > 0,
        || {
            format!(
                "box scan: {} files, {} dirs, +{} ~{} -{} ({} placeholders)",
                stats.total_files,
                stats.total_dirs,
                stats.added,
                stats.modified,
                stats.removed,
                stats.placeholders,
            )
        },
    ))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    let granted = find_box_root().is_some();
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(granted),
        // FDA is technically required to read ~/Library/CloudStorage; in
        // practice, the Trove app already holds it, so this is never a blocker.
        required: false,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    let stream = vault.stream(&format!("{VAULT_PATH}/events"), Partition::Month);
    if let Ok(parts) = stream.partitions() {
        if let Some(latest) = parts.last() {
            if let Ok(events) = stream.read::<cloud_folder::ChangeEvent>(latest) {
                if let Some(last) = events.last() {
                    return Some(last.ts.clone());
                }
            }
        }
    }
    crate::registry::file_mtime(
        &vault.root().join(format!("{VAULT_PATH}/snapshot.jsonl")),
    )
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "box",
        name: "Box",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Indexes your Box Drive folder — file names, paths, sizes, \
                      and modification times — by scanning the local mirror. \
                      Online-only files are noted as placeholders; their \
                      contents are never read or downloaded.",
        domain: "files",
        vault_path: "files/box/",
        toggleable: true,
        setup: &[
            "Install the Box Drive desktop app and let it sync at least one file.",
            "Uses the Full Disk Access grant Trove already holds — no additional permission needed.",
        ],
        caveats: "Probes ~/Library/CloudStorage/Box-Box (modern File Provider, \
                  the real directory Box Drive creates) then ~/Box (legacy). \
                  Online-only placeholder files are recorded with \
                  placeholder=true and are never downloaded. Box API OAuth (for \
                  users without the desktop app) is deferred pending community \
                  demand — this is a niche personal-Mac use case.",
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
            "trove-box-home-{}-{}-{tag}",
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

    // -----------------------------------------------------------------------
    // Path probe tests.

    /// The real-world Box Drive File Provider directory is "Box-Box" (not bare
    /// "Box"). This is the primary path and must be found first.
    #[test]
    fn probe_finds_box_box_canonical() {
        let home = temp_home("boxbox");
        let box_dir = home.join("Library/CloudStorage/Box-Box");
        fs::create_dir_all(&box_dir).unwrap();

        let found = probe_box_root(&home);
        assert_eq!(found.as_deref(), Some(box_dir.as_path()), "Box-Box must be the primary match");

        let _ = fs::remove_dir_all(&home);
    }

    /// Box-Box must win over an alphabetically-earlier Box* directory (e.g.
    /// "Box-Archive") — the canonical preference must not be defeated by sort order.
    #[test]
    fn probe_prefers_box_box_over_alphabetically_earlier_glob() {
        let home = temp_home("coll");
        let cs = home.join("Library/CloudStorage");
        // "Box-Archive" sorts before "Box-Box" alphabetically, but Box-Box is
        // canonical and must be preferred.
        fs::create_dir_all(cs.join("Box-Archive")).unwrap();
        let box_box = cs.join("Box-Box");
        fs::create_dir_all(&box_box).unwrap();

        let found = probe_box_root(&home);
        assert_eq!(found.as_deref(), Some(box_box.as_path()),
            "Box-Box must win over alphabetically-earlier Box-Archive");

        let _ = fs::remove_dir_all(&home);
    }

    /// Bare "Box" is kept as a secondary option for forward compatibility.
    #[test]
    fn probe_finds_cloud_storage_box() {
        let home = temp_home("cs");
        let box_dir = home.join("Library/CloudStorage/Box");
        fs::create_dir_all(&box_dir).unwrap();

        let found = probe_box_root(&home);
        assert_eq!(found.as_deref(), Some(box_dir.as_path()));

        let _ = fs::remove_dir_all(&home);
    }

    /// Box-Box takes priority over bare Box when both are present.
    #[test]
    fn probe_prefers_box_box_over_bare_box() {
        let home = temp_home("bbpref");
        let cs = home.join("Library/CloudStorage");
        let box_box = cs.join("Box-Box");
        let bare = cs.join("Box");
        fs::create_dir_all(&box_box).unwrap();
        fs::create_dir_all(&bare).unwrap();

        let found = probe_box_root(&home);
        assert_eq!(found.as_deref(), Some(box_box.as_path()),
            "Box-Box must be preferred over bare Box");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_finds_legacy_box() {
        let home = temp_home("leg");
        let box_dir = home.join("Box");
        fs::create_dir_all(&box_dir).unwrap();

        let found = probe_box_root(&home);
        assert_eq!(found.as_deref(), Some(box_dir.as_path()));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_prefers_cloud_storage_over_legacy() {
        let home = temp_home("pref");
        let cloud = home.join("Library/CloudStorage/Box-Box");
        let legacy = home.join("Box");
        fs::create_dir_all(&cloud).unwrap();
        fs::create_dir_all(&legacy).unwrap();

        let found = probe_box_root(&home);
        assert_eq!(found.as_deref(), Some(cloud.as_path()), "cloud preferred over legacy");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_returns_none_when_nothing_found() {
        let home = temp_home("none");
        let found = probe_box_root(&home);
        assert!(found.is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_skips_old_and_timestamped_dirs() {
        let home = temp_home("skip");
        let cs = home.join("Library/CloudStorage");
        fs::create_dir_all(cs.join("Box (Old)")).unwrap();
        fs::create_dir_all(cs.join("Box (2024-01-01)")).unwrap();

        let found = probe_box_root(&home);
        assert!(found.is_none(), "timestamped and Old dirs must be skipped");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_glob_fallback_finds_other_box_variant() {
        let home = temp_home("glob");
        let cs = home.join("Library/CloudStorage");
        // A variant that isn't "Box-Box" or "Box" but starts with "Box".
        let box_variant = cs.join("Box (Enterprise)");
        fs::create_dir_all(&box_variant).unwrap();

        let found = probe_box_root(&home);
        assert_eq!(found.as_deref(), Some(box_variant.as_path()));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn timestamped_backup_heuristic() {
        assert!(looks_like_timestamped_backup("Box (2024-01-01)"));
        assert!(looks_like_timestamped_backup("Box (20240101)"));
        assert!(!looks_like_timestamped_backup("Box (Personal)"));
        assert!(!looks_like_timestamped_backup("Box (Enterprise)"));
        assert!(!looks_like_timestamped_backup("Box"));
        assert!(!looks_like_timestamped_backup("Box-Personal"));
    }

    // -----------------------------------------------------------------------
    // Integration smoke tests: scan+diff via cloud_folder, injecting the
    // Box Drive root directly (no TROVE_HOME manipulation — avoids parallel
    // env-var races between tests).

    fn temp_vault(tag: &str) -> crate::vault::Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-box-vault-{}-{}-{tag}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos() as u64
        ));
        let _ = fs::remove_dir_all(&dir);
        crate::vault::Vault::open_or_create(dir).unwrap()
    }

    /// Thin wrapper around scan_and_diff that takes the root directly — lets
    /// tests bypass the find_box_root() probe without touching process env.
    fn scan_box(vault: &crate::vault::Vault, root: &Path) -> Result<cloud_folder::ScanStats> {
        cloud_folder::scan_and_diff(vault, VAULT_PATH, root, cloud_folder::real_is_dataless)
    }

    #[test]
    fn collect_scan_baseline_and_event() {
        let home = temp_home("collect");
        // Use the real-world Box-Box directory name (Box Drive File Provider).
        let box_dir = home.join("Library/CloudStorage/Box-Box");
        fs::create_dir_all(&box_dir).unwrap();

        // Seed a file.
        fs::write(box_dir.join("report.pdf"), b"pdf content").unwrap();
        fs::create_dir(box_dir.join("projects")).unwrap();
        fs::write(box_dir.join("projects/plan.docx"), b"docx content").unwrap();

        let vault = temp_vault("collect");

        // First scan = silent baseline.
        let stats1 = scan_box(&vault, &box_dir).unwrap();
        assert!(stats1.baseline, "first scan should be a baseline");
        assert_eq!(stats1.added, 0, "baseline emits no events");

        // Second scan with a new file added.
        fs::write(box_dir.join("notes.txt"), b"hello").unwrap();
        let stats2 = scan_box(&vault, &box_dir).unwrap();
        assert_eq!(stats2.added, 1, "second scan notes 1 added file");
        assert!(!stats2.baseline);

        // Verify vault has events.
        let stream = vault.stream(&format!("{VAULT_PATH}/events"), Partition::Month);
        let parts = stream.partitions().unwrap();
        assert!(!parts.is_empty(), "events should be written after file addition");
        let events: Vec<cloud_folder::ChangeEvent> = stream.read(&parts[0]).unwrap();
        assert!(events.iter().any(|e| e.name == "notes.txt" && e.kind == "added"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_finds_nothing_when_box_not_installed() {
        let home = temp_home("err");
        // No Box directory anywhere.
        let found = probe_box_root(&home);
        assert!(found.is_none(), "should find nothing when Box Drive is not installed");
        let _ = fs::remove_dir_all(&home);
    }
}
