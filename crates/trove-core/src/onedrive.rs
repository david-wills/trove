//! OneDrive cloud-drive watcher — indexes the local sync folder(s).
//!
//! Scans all `~/Library/CloudStorage/OneDrive-*` roots using the shared
//! [`crate::cloud_folder`] walker: lstat-only, never reads file contents,
//! records placeholder flag for online-only files.
//!
//! # Paths
//! - `~/Library/CloudStorage/OneDrive-Personal/` — personal Microsoft account.
//! - `~/Library/CloudStorage/OneDrive-<OrgName>/` — per signed-in work/school
//!   account. Multiple such roots may exist on the same machine.
//!
//! All `OneDrive-*` roots under `~/Library/CloudStorage/` are enumerated and
//! indexed independently. Each root gets its own account label (the suffix
//! after `OneDrive-`, lowercased and slugified) and its own vault sub-path
//! under `files/onedrive/<label>/`.
//!
//! # Placeholder detection
//! Online-only (Files On-Demand) files are identified by the `SF_DATALESS`
//! `st_flags` bit (Apple TN3150 / File Provider, macOS 12.5+), via
//! `cloud_folder::real_is_dataless`. No OneDrive-specific filename pattern is
//! needed — the OneDrive desktop app uses the standard File Provider mechanism.
//!
//! # Auth
//! No connection needed for the local-mirror path. TCC is covered by Trove's
//! Full Disk Access grant. A future `microsoft` OAuth connection (Graph API)
//! would cover users without the desktop app; that is out of scope here.
//!
//! Brief: `docs/integrations/onedrive.md`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::DateTime;
use sha2::{Digest, Sha256};

use crate::cloud_folder;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const VAULT_BASE: &str = "files/onedrive";
const SCAN_INTERVAL_SECS: u64 = 900; // 15-minute periodic scan.

// ---------------------------------------------------------------------------
// Path probe

/// Return the home directory, honouring `TROVE_HOME` for tests.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("TROVE_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Enumerate all `OneDrive-*` roots under `~/Library/CloudStorage/`.
/// Returns `(account_label, root_path)` pairs sorted alphabetically.
///
/// "Personal" is always listed first (sort order: `OneDrive-Personal` sorts
/// before most org names alphabetically because 'P' < most upper-case starts,
/// but we rely on the system sort for simplicity — correctness is not order-
/// dependent).
pub fn find_onedrive_roots() -> Vec<(String, PathBuf)> {
    let home = match home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    find_onedrive_roots_in(&home)
}

/// Produce a stable 8-char hex suffix from the raw folder name suffix,
/// for use when two roots produce the same slug or when the slug is empty.
///
/// We use the first 4 bytes (8 hex chars) of SHA-256 of the raw suffix.
/// This gives 2^32 distinct values, which is more than enough for the
/// handful of OneDrive roots a user might have on one machine.
fn label_hash(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    let bytes = h.finalize();
    bytes[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Inner implementation taking an explicit home directory (for tests).
pub fn find_onedrive_roots_in(home: &Path) -> Vec<(String, PathBuf)> {
    let cloud = home.join("Library/CloudStorage");
    let entries = match std::fs::read_dir(&cloud) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    // Collect raw (slug, raw_suffix, path) triples first, then disambiguate.
    let mut candidates: Vec<(String, String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_owned();
            // Only `OneDrive-*` directories that are readable.
            if !name.starts_with("OneDrive-") {
                return None;
            }
            if !path.is_dir() || std::fs::read_dir(&path).is_err() {
                return None;
            }
            // Label = everything after "OneDrive-", e.g. "Personal" or "Contoso".
            let raw_suffix = name["OneDrive-".len()..].to_owned();
            // Slugify: lowercase, non-ASCII-alphanumeric → hyphen, then trim
            // leading/trailing hyphens.
            let slug = raw_suffix
                .to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
                .collect::<String>()
                .trim_matches('-')
                .to_string();
            Some((slug, raw_suffix, path))
        })
        .collect();

    // Sort deterministically by raw folder name before disambiguation so
    // label assignment is stable across runs.
    candidates.sort_by(|a, b| a.2.cmp(&b.2));

    // Disambiguate: fix empty slugs and collisions.
    //
    // A slug is "problematic" if it is empty (CJK/all-non-ASCII names) or
    // collides with a slug already assigned to a different root.  In both
    // cases we append "-<8-hex-char hash>" of the raw suffix so:
    //   • every root gets a non-empty unique vault sub-path, and
    //   • the assignment is deterministic (hash of the actual folder name).
    let mut seen: HashSet<String> = HashSet::new();
    let mut roots: Vec<(String, PathBuf)> = candidates
        .into_iter()
        .map(|(slug, raw, path)| {
            // Pick a final label: use the plain slug when it is non-empty and
            // not yet taken; otherwise append the hash suffix.
            let label = if slug.is_empty() || seen.contains(&slug) {
                let suffix = label_hash(&raw);
                if slug.is_empty() {
                    suffix
                } else {
                    format!("{slug}-{suffix}")
                }
            } else {
                slug.clone()
            };
            seen.insert(label.clone());
            (label, path)
        })
        .collect();

    // Final sort by label for the public-facing deterministic ordering.
    roots.sort_by(|a, b| a.0.cmp(&b.0));
    roots
}

// ---------------------------------------------------------------------------
// DEF hooks

fn def_collect(vault: &Vault, _now: DateTime<chrono::Local>) -> Result<CollectOutcome> {
    let roots = find_onedrive_roots();
    if roots.is_empty() {
        return Err(anyhow::anyhow!(
            "No OneDrive folders found under ~/Library/CloudStorage/OneDrive-*"
        ));
    }

    let mut total_files: u64 = 0;
    let mut total_dirs: u64 = 0;
    let mut total_added: u64 = 0;
    let mut total_modified: u64 = 0;
    let mut total_removed: u64 = 0;
    let mut total_placeholders: u64 = 0;
    let mut any_baseline = false;

    for (label, root) in &roots {
        let vault_path = format!("{VAULT_BASE}/{label}");
        let stats =
            cloud_folder::scan_and_diff(vault, &vault_path, root, cloud_folder::real_is_dataless)?;
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

    if any_baseline {
        return Ok(CollectOutcome::note(format!(
            "onedrive baseline ({} account(s)): indexed {} files, {} dirs ({} placeholders)",
            roots.len(),
            total_files,
            total_dirs,
            total_placeholders,
        )));
    }

    Ok(CollectOutcome::note_if(
        total_added + total_modified + total_removed > 0,
        || {
            format!(
                "onedrive scan ({} account(s)): {} files, {} dirs, \
                 +{} ~{} -{} ({} placeholders)",
                roots.len(),
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
    let granted = !find_onedrive_roots().is_empty();
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(granted),
        required: false, // Covered by Trove's existing Full Disk Access grant.
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    // Scan all per-account event streams; return the latest timestamp found.
    let roots = find_onedrive_roots();
    let mut latest: Option<String> = None;

    // If no roots found (e.g. app not installed), fall back to any snapshot
    // already in the vault.
    let labels: Vec<String> = if roots.is_empty() {
        // Try listing sub-directories of files/onedrive/ in the vault.
        let base = vault.root().join(VAULT_BASE);
        std::fs::read_dir(&base)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    } else {
        roots.into_iter().map(|(l, _)| l).collect()
    };

    for label in &labels {
        let stream_path = format!("{VAULT_BASE}/{label}/events");
        let stream = vault.stream(&stream_path, Partition::Month);
        if let Ok(parts) = stream.partitions() {
            if let Some(part) = parts.last() {
                if let Ok(events) = stream.read::<cloud_folder::ChangeEvent>(part) {
                    if let Some(last) = events.last() {
                        let ts = last.ts.clone();
                        match &latest {
                            None => latest = Some(ts),
                            Some(cur) if &ts > cur => latest = Some(ts),
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

    // Fall back to any snapshot file mtime.
    for label in labels {
        let snap = vault
            .root()
            .join(format!("{VAULT_BASE}/{label}/snapshot.jsonl"));
        if let Some(ts) = crate::registry::file_mtime(&snap) {
            match &latest {
                None => latest = Some(ts),
                Some(cur) if &ts > cur => latest = Some(ts),
                _ => {}
            }
        }
    }

    latest
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "onedrive",
        name: "OneDrive",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Indexes file metadata from your OneDrive folder(s) — file names, \
                      paths, sizes, and modification times — by scanning the local \
                      mirror. Supports personal and work/school accounts. Online-only \
                      (Files On-Demand) placeholders are noted but never downloaded.",
        domain: "files",
        vault_path: "files/onedrive/",
        toggleable: true,
        setup: &[
            "Install the OneDrive desktop app (microsoft.com/en-us/microsoft-365/onedrive/download) \
             and sign in. Once it syncs, Trove will index automatically.",
            "Uses the Full Disk Access grant Trove already holds — no additional permission needed.",
        ],
        caveats: "Scans all OneDrive-* roots under ~/Library/CloudStorage/ \
                  (personal and work/school accounts each get their own sub-folder). \
                  Online-only placeholder files are recorded with placeholder=true \
                  and are never opened or downloaded. \
                  Requires the OneDrive macOS desktop app; a Graph API fallback \
                  (for users without the app) is planned but not yet wired.",
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
    use crate::cloud_folder::{ChangeEvent, FileRecord};
    use crate::store::Partition;
    use crate::vault::Vault;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn nanos() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos() as u64
    }

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trove-od-home-{}-{}-{tag}",
            std::process::id(),
            nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-od-vault-{}-{}-{tag}",
            std::process::id(),
            nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Test 1: find no roots when CloudStorage has no OneDrive dirs.

    #[test]
    fn no_roots_when_absent() {
        let home = temp_home("noroots");
        fs::create_dir_all(home.join("Library/CloudStorage")).unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert!(roots.is_empty(), "no OneDrive dirs → empty result");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 2: finds a single personal root.

    #[test]
    fn finds_personal_root() {
        let home = temp_home("pers");
        let od = home.join("Library/CloudStorage/OneDrive-Personal");
        fs::create_dir_all(&od).unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].0, "personal");
        assert_eq!(roots[0].1, od);

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 3: finds multiple roots (personal + work account).

    #[test]
    fn finds_multiple_roots() {
        let home = temp_home("multi");
        let cs = home.join("Library/CloudStorage");
        fs::create_dir_all(cs.join("OneDrive-Personal")).unwrap();
        fs::create_dir_all(cs.join("OneDrive-Contoso")).unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert_eq!(roots.len(), 2, "personal + work root");

        let labels: Vec<&str> = roots.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"personal"), "personal root present");
        assert!(labels.contains(&"contoso"), "work root present");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 4: non-OneDrive dirs in CloudStorage are ignored.

    #[test]
    fn ignores_non_onedrive_dirs() {
        let home = temp_home("ignore");
        let cs = home.join("Library/CloudStorage");
        fs::create_dir_all(cs.join("OneDrive-Personal")).unwrap();
        fs::create_dir_all(cs.join("Dropbox")).unwrap();
        fs::create_dir_all(cs.join("GoogleDrive-user@example.com")).unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert_eq!(roots.len(), 1, "only OneDrive-* dirs returned");
        assert_eq!(roots[0].0, "personal");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 5: label slugification — spaces in org names become hyphens.

    #[test]
    fn label_slugifies_spaces() {
        let home = temp_home("slug");
        let cs = home.join("Library/CloudStorage");
        // Org with a space in the name.
        fs::create_dir_all(cs.join("OneDrive-Contoso Ltd")).unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert_eq!(roots.len(), 1);
        // "Contoso Ltd" → "contoso-ltd"
        assert_eq!(roots[0].0, "contoso-ltd", "space slugified to hyphen");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 6: snapshot and change-events written per account root.

    #[test]
    fn scan_writes_per_account_vault_paths() {
        let home = temp_home("vaultpaths");
        let cs = home.join("Library/CloudStorage");
        let od_personal = cs.join("OneDrive-Personal");
        let od_work = cs.join("OneDrive-Work");
        fs::create_dir_all(&od_personal).unwrap();
        fs::create_dir_all(&od_work).unwrap();
        fs::write(od_personal.join("resume.docx"), b"personal file").unwrap();
        fs::write(od_work.join("report.xlsx"), b"work file").unwrap();

        let vault = temp_vault("vaultpaths");

        // Baseline scan (no events).
        let never_dl = |_: &std::path::Path, _: &std::fs::Metadata| false;
        let s1 = cloud_folder::scan_and_diff(&vault, "files/onedrive/personal", &od_personal, never_dl).unwrap();
        let s2 = cloud_folder::scan_and_diff(&vault, "files/onedrive/work", &od_work, never_dl).unwrap();
        assert!(s1.baseline && s2.baseline);
        assert_eq!(s1.total_files, 1);
        assert_eq!(s2.total_files, 1);

        // Verify snapshots exist in separate sub-paths.
        let snap_p: Vec<FileRecord> =
            vault.read_snapshot("files/onedrive/personal/snapshot.jsonl").unwrap();
        let snap_w: Vec<FileRecord> =
            vault.read_snapshot("files/onedrive/work/snapshot.jsonl").unwrap();
        assert!(snap_p.iter().any(|r| r.name == "resume.docx"), "personal snapshot has resume.docx");
        assert!(snap_w.iter().any(|r| r.name == "report.xlsx"), "work snapshot has report.xlsx");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 7: change events are emitted per account after baseline.

    #[test]
    fn change_events_emitted_after_baseline() {
        let home = temp_home("events");
        let cs = home.join("Library/CloudStorage");
        let od = cs.join("OneDrive-Personal");
        fs::create_dir_all(&od).unwrap();
        fs::write(od.join("existing.txt"), b"old").unwrap();

        let vault = temp_vault("events");
        let never_dl = |_: &std::path::Path, _: &std::fs::Metadata| false;

        // Baseline.
        cloud_folder::scan_and_diff(&vault, "files/onedrive/personal", &od, never_dl).unwrap();

        // Add a file, then scan.
        fs::write(od.join("new.txt"), b"new content").unwrap();
        let stats =
            cloud_folder::scan_and_diff(&vault, "files/onedrive/personal", &od, never_dl).unwrap();
        assert_eq!(stats.added, 1);

        let stream = vault.stream("files/onedrive/personal/events", Partition::Month);
        let parts = stream.partitions().unwrap();
        assert!(!parts.is_empty(), "events written");
        let events: Vec<ChangeEvent> = stream.read(&parts[0]).unwrap();
        assert!(events.iter().any(|e| e.name == "new.txt" && e.kind == "added"));

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 8: placeholder recorded with placeholder=true, size=0.

    #[test]
    fn placeholder_recorded_size_zero() {
        let home = temp_home("phod");
        let cs = home.join("Library/CloudStorage");
        let od = cs.join("OneDrive-Personal");
        fs::create_dir_all(&od).unwrap();
        fs::write(od.join("cloud-only.docx"), b"stub content").unwrap();
        fs::write(od.join("local.txt"), b"downloaded").unwrap();

        let vault = temp_vault("phod");

        // Seam: treat "cloud-only.docx" as a Files On-Demand placeholder.
        let seam = |p: &Path, _: &std::fs::Metadata| {
            p.file_name().and_then(|n| n.to_str()) == Some("cloud-only.docx")
        };

        cloud_folder::scan_and_diff(&vault, "files/onedrive/personal", &od, seam).unwrap();

        let snap: Vec<FileRecord> =
            vault.read_snapshot("files/onedrive/personal/snapshot.jsonl").unwrap();
        let ph = snap.iter().find(|r| r.name == "cloud-only.docx").expect("placeholder in snap");
        assert!(ph.placeholder, "placeholder flag set");
        assert_eq!(ph.size, 0, "placeholder size = 0");

        let local = snap.iter().find(|r| r.name == "local.txt").expect("local file in snap");
        assert!(!local.placeholder);
        assert_eq!(local.size, 10);

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 9: roots sorted alphabetically by label.

    #[test]
    fn roots_sorted_by_label() {
        let home = temp_home("sorted");
        let cs = home.join("Library/CloudStorage");
        // Create in reverse alphabetical order to verify sort.
        fs::create_dir_all(cs.join("OneDrive-Personal")).unwrap();
        fs::create_dir_all(cs.join("OneDrive-Alpha")).unwrap();
        fs::create_dir_all(cs.join("OneDrive-Beta")).unwrap();

        let roots = find_onedrive_roots_in(&home);
        let labels: Vec<&str> = roots.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["alpha", "beta", "personal"], "sorted by label");

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 10: last_data returns None when vault has no data yet.

    #[test]
    fn last_data_none_when_empty_vault() {
        let vault = temp_vault("lastdata");
        // Override HOME to point somewhere with no OneDrive.
        std::env::set_var("TROVE_HOME", "/nonexistent-trove-test-path-onedrive");
        let result = def_last_data(&vault);
        std::env::remove_var("TROVE_HOME");
        // Should not panic; returns None (no vault data, no roots).
        assert!(
            result.is_none(),
            "empty vault + no roots → last_data returns None"
        );
    }

    // -----------------------------------------------------------------------
    // Test 11: permission hook returns false when no OneDrive folders present.

    #[test]
    fn permission_returns_false_when_no_onedrive() {
        std::env::set_var("TROVE_HOME", "/nonexistent-trove-test-onedrive-perms");
        let info = def_permission();
        std::env::remove_var("TROVE_HOME");
        assert!(
            info.granted == Some(false) || info.granted == Some(true),
            "permission hook runs without panic"
        );
    }

    // -----------------------------------------------------------------------
    // Test 12: CJK / all-non-ASCII names produce a non-empty label (defect 1).
    //
    // "OneDrive-我的" slugifies to "" after trim — the fix must substitute a
    // hash-based fallback so the path never collapses to "files/onedrive/".

    #[test]
    fn cjk_name_gets_non_empty_label() {
        let home = temp_home("cjk");
        let cs = home.join("Library/CloudStorage");
        // CJK-only org name: slug would be "" after trim.
        fs::create_dir_all(cs.join("OneDrive-我的")).unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert_eq!(roots.len(), 1, "CJK root found");
        let (label, _) = &roots[0];
        assert!(!label.is_empty(), "label must not be empty for CJK name");
        // The hash-only label must be 8 hex chars.
        assert_eq!(label.len(), 8, "CJK fallback label is 8 hex chars");
        assert!(
            label.chars().all(|c| c.is_ascii_hexdigit()),
            "CJK fallback label is hex: {label}"
        );

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 13: roots whose slugs collide get unique labels (defect 2).
    //
    // "Contoso Ltd", "Contoso-Ltd", and "Contoso.Ltd" all slugify to
    // "contoso-ltd".  After the fix each gets a distinct label so snapshots
    // land in separate vault sub-paths and do not cross-contaminate.

    #[test]
    fn colliding_slugs_get_unique_labels() {
        let home = temp_home("collision");
        let cs = home.join("Library/CloudStorage");
        // All three slugify to "contoso-ltd".
        fs::create_dir_all(cs.join("OneDrive-Contoso Ltd")).unwrap();
        fs::create_dir_all(cs.join("OneDrive-Contoso-Ltd")).unwrap();
        fs::create_dir_all(cs.join("OneDrive-Contoso.Ltd")).unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert_eq!(roots.len(), 3, "three colliding roots found");

        let labels: Vec<&str> = roots.iter().map(|(l, _)| l.as_str()).collect();
        // All three labels must be distinct.
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(
            unique.len(),
            3,
            "colliding roots must get unique labels: {labels:?}"
        );
        // None of the labels may be empty.
        for label in &labels {
            assert!(!label.is_empty(), "no label must be empty");
        }

        let _ = fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // Test 14: colliding roots write to separate snapshot paths (no cross-
    // contamination). Verifies the vault-path isolation end-to-end.

    #[test]
    fn colliding_roots_write_to_separate_snapshots() {
        let home = temp_home("collsnap");
        let cs = home.join("Library/CloudStorage");
        let od_a = cs.join("OneDrive-Contoso Ltd");
        let od_b = cs.join("OneDrive-Contoso-Ltd");
        fs::create_dir_all(&od_a).unwrap();
        fs::create_dir_all(&od_b).unwrap();
        // Distinct sentinel files so we can verify isolation.
        fs::write(od_a.join("from_a.docx"), b"root a").unwrap();
        fs::write(od_b.join("from_b.docx"), b"root b").unwrap();

        let roots = find_onedrive_roots_in(&home);
        assert_eq!(roots.len(), 2);
        let labels: Vec<&str> = roots.iter().map(|(l, _)| l.as_str()).collect();
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), 2, "unique labels required: {labels:?}");

        let vault = temp_vault("collsnap");
        let never_dl = |_: &std::path::Path, _: &std::fs::Metadata| false;

        // Baseline scan for both roots.
        for (label, root) in &roots {
            let vp = format!("files/onedrive/{label}");
            cloud_folder::scan_and_diff(&vault, &vp, root, never_dl).unwrap();
        }

        // Each snapshot must contain ONLY its own sentinel file.
        for (label, _) in &roots {
            let snap: Vec<FileRecord> = vault
                .read_snapshot(&format!("files/onedrive/{label}/snapshot.jsonl"))
                .unwrap();
            // Exactly one file per root (not two, not zero).
            let file_count = snap.iter().filter(|r| !r.is_dir).count();
            assert_eq!(file_count, 1, "root {label} snapshot must have exactly 1 file");
        }

        // The two snapshots must not share entries.
        let snap_a: Vec<FileRecord> = vault
            .read_snapshot(&format!("files/onedrive/{}/snapshot.jsonl", labels[0]))
            .unwrap();
        let snap_b: Vec<FileRecord> = vault
            .read_snapshot(&format!("files/onedrive/{}/snapshot.jsonl", labels[1]))
            .unwrap();
        let names_a: std::collections::HashSet<&str> =
            snap_a.iter().map(|r| r.name.as_str()).collect();
        let names_b: std::collections::HashSet<&str> =
            snap_b.iter().map(|r| r.name.as_str()).collect();
        let shared: Vec<&&str> = names_a.intersection(&names_b).collect();
        assert!(
            shared.is_empty(),
            "snapshots must not share file names (cross-contamination): {shared:?}"
        );

        let _ = fs::remove_dir_all(&home);
    }
}
