//! Dropbox cloud storage — file-tree index from the local Dropbox mirror.
//!
//! Scans the Dropbox local folder (installed by the Dropbox desktop app) using
//! the shared [`crate::cloud_folder`] walker: lstat-only, never reads file
//! contents, records placeholder flag for online-only files.
//!
//! Path probe (priority order):
//! 1. `~/Library/CloudStorage/Dropbox`   (File Provider, macOS 12.5+)
//! 2. `~/Library/CloudStorage/Dropbox (Personal)`
//! 3. Any `~/Library/CloudStorage/Dropbox (*)`  — prefer "(Personal)", else
//!    first alphabetical match, skipping names that contain "Old" or look like
//!    timestamped backups.
//! 4. `~/Dropbox`  (legacy pre-File-Provider path)
//!
//! `TROVE_HOME` env var overrides `HOME` in tests so the probe runs in a
//! controlled temp dir.
//!
//! Brief: `docs/integrations/dropbox.md`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::DateTime;

use crate::cloud_folder;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const VAULT_PATH: &str = "files/dropbox";
const SCAN_INTERVAL_SECS: u64 = 900; // 15-minute periodic scan.

// ---------------------------------------------------------------------------
// Path probe

/// Resolve the Dropbox root from the environment. Checks `TROVE_HOME` first
/// (for tests), then falls back to `HOME`. Returns `None` if no Dropbox
/// folder is detected.
pub fn find_dropbox_root() -> Option<PathBuf> {
    let home = home_dir()?;
    probe_dropbox_root(&home)
}

/// `HOME` override for tests via `TROVE_HOME`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("TROVE_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Walk the priority list until a readable Dropbox directory is found.
fn probe_dropbox_root(home: &Path) -> Option<PathBuf> {
    let cloud = home.join("Library/CloudStorage");

    // 1. Exact canonical path (modern File Provider).
    let exact = cloud.join("Dropbox");
    if is_accessible_dir(&exact) {
        return Some(exact);
    }

    // 2. "(Personal)" variant.
    let personal = cloud.join("Dropbox (Personal)");
    if is_accessible_dir(&personal) {
        return Some(personal);
    }

    // 3. Any `Dropbox (*)` glob, preferring "(Personal)", excluding "Old" and
    //    timestamp-shaped names like "(2024-01-01)".
    if let Some(matched) = best_dropbox_glob(&cloud) {
        return Some(matched);
    }

    // 4. Legacy `~/Dropbox`.
    let legacy = home.join("Dropbox");
    if is_accessible_dir(&legacy) {
        return Some(legacy);
    }

    None
}

/// Scan `~/Library/CloudStorage/` for entries whose name starts with "Dropbox"
/// and pick the best one (excluding known noise patterns). Returns `None` when
/// nothing matches.
fn best_dropbox_glob(cloud_storage: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(cloud_storage).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("Dropbox")
                && p.is_dir()
                // Skip "Old" variants (e.g. "Dropbox (Old)") and timestamp-shaped
                // names like "Dropbox (2024-01-01)" — they're migration leftovers.
                && !name.contains("Old")
                && !looks_like_timestamped_backup(name)
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }
    // Sort for determinism; this places "(Personal)" before plain alphabetical
    // alt names and picks the first readable one.
    candidates.sort();
    candidates.into_iter().find(|p| is_accessible_dir(p))
}

/// Heuristic: does the directory name look like a timestamped backup?
/// Matches patterns like "Dropbox (2024-01-01)" or "Dropbox (20240101)".
fn looks_like_timestamped_backup(name: &str) -> bool {
    // Extract what's inside the parentheses, if present.
    if let (Some(open), Some(close)) = (name.find('('), name.rfind(')')) {
        if open < close {
            let inner = &name[open + 1..close];
            // 6+ digit run or date-shaped "YYYY-MM-DD".
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
    let root = find_dropbox_root()
        .ok_or_else(|| anyhow::anyhow!("Dropbox folder not found"))?;
    let stats = cloud_folder::scan_and_diff(
        vault,
        VAULT_PATH,
        &root,
        cloud_folder::real_is_dataless,
    )?;
    // First-ever scan is a silent baseline (no change events) — but it captured
    // the whole tree, so note that. Otherwise note only when something changed.
    if stats.baseline {
        return Ok(CollectOutcome::note(format!(
            "dropbox baseline: indexed {} files, {} dirs ({} placeholders)",
            stats.total_files, stats.total_dirs, stats.placeholders,
        )));
    }
    Ok(CollectOutcome::note_if(
        stats.added + stats.modified + stats.removed > 0,
        || {
            format!(
                "dropbox scan: {} files, {} dirs, +{} ~{} -{} ({} placeholders)",
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
    let granted = find_dropbox_root().is_some();
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(granted),
        required: false, // The Dropbox folder is readable without FDA on most setups.
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    // Try the most recent event timestamp from the events stream.
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
    // Fall back to snapshot file mtime.
    crate::registry::file_mtime(
        &vault.root().join(format!("{VAULT_PATH}/snapshot.jsonl")),
    )
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "dropbox",
        name: "Dropbox",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Indexes your Dropbox folder — file names, paths, sizes, \
                      and modification times — by scanning the local mirror. \
                      Online-only files are noted as placeholders; their \
                      contents are never read or downloaded.",
        domain: "files",
        vault_path: "files/dropbox/",
        toggleable: true,
        setup: &[
            "Install the Dropbox desktop app and let it sync at least one file.",
            "Uses the Full Disk Access grant Trove already holds — no additional permission needed.",
        ],
        caveats: "Probes ~/Library/CloudStorage/Dropbox (modern) then ~/Dropbox \
                  (legacy). Online-only placeholder files are recorded with \
                  placeholder=true and are never downloaded. The Dropbox API \
                  fallback (for users without the desktop app) is planned but \
                  not yet wired.",
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
            "trove-dbx-home-{}-{}-{tag}",
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
    // Test 5 (path probe): CloudStorage/Dropbox → found.

    #[test]
    fn probe_finds_cloud_storage_dropbox() {
        let home = temp_home("cs");
        let dropbox = home.join("Library/CloudStorage/Dropbox");
        fs::create_dir_all(&dropbox).unwrap();

        let found = probe_dropbox_root(&home);
        assert_eq!(found.as_deref(), Some(dropbox.as_path()));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_finds_dropbox_personal_variant() {
        let home = temp_home("pers");
        let dropbox = home.join("Library/CloudStorage/Dropbox (Personal)");
        fs::create_dir_all(&dropbox).unwrap();

        let found = probe_dropbox_root(&home);
        assert_eq!(found.as_deref(), Some(dropbox.as_path()));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_finds_legacy_dropbox() {
        let home = temp_home("leg");
        let dropbox = home.join("Dropbox");
        fs::create_dir_all(&dropbox).unwrap();

        let found = probe_dropbox_root(&home);
        assert_eq!(found.as_deref(), Some(dropbox.as_path()));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_prefers_cloud_storage_over_legacy() {
        let home = temp_home("pref");
        let cloud = home.join("Library/CloudStorage/Dropbox");
        let legacy = home.join("Dropbox");
        fs::create_dir_all(&cloud).unwrap();
        fs::create_dir_all(&legacy).unwrap();

        let found = probe_dropbox_root(&home);
        assert_eq!(found.as_deref(), Some(cloud.as_path()), "cloud preferred over legacy");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_returns_none_when_nothing_found() {
        let home = temp_home("none");
        // No Dropbox folder anywhere.
        let found = probe_dropbox_root(&home);
        assert!(found.is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_skips_old_and_timestamped_dirs() {
        let home = temp_home("skip");
        let cs = home.join("Library/CloudStorage");
        fs::create_dir_all(cs.join("Dropbox (Old)")).unwrap();
        fs::create_dir_all(cs.join("Dropbox (2024-01-01)")).unwrap();

        let found = probe_dropbox_root(&home);
        // None of the timestamped/Old dirs should match.
        assert!(found.is_none(), "timestamped and Old dirs must be skipped");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn timestamped_backup_heuristic() {
        assert!(looks_like_timestamped_backup("Dropbox (2024-01-01)"));
        assert!(looks_like_timestamped_backup("Dropbox (20240101)"));
        assert!(!looks_like_timestamped_backup("Dropbox (Personal)"));
        assert!(!looks_like_timestamped_backup("Dropbox (Work)"));
        assert!(!looks_like_timestamped_backup("Dropbox"));
    }
}
