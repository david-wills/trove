//! VS Code (and VS Code-fork) recent workspace / file activity.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/vscode.md.
//!
//! A **Periodic** local-file collector.  Reads the `history.recentlyOpenedPathsList`
//! key from `~/Library/Application Support/<product>/User/globalStorage/state.vscdb`
//! via a **copy-then-read** pattern (the same pattern as `browser.rs` and
//! `imessage.rs`) so VS Code's write-lock is never contested.
//!
//! **`developer/` is raw-only** (taxonomy decision): no contract, no
//! normalized struct — this module owns its row shape.
//!
//! ## Schema (verified against real Cursor 1.x state.vscdb)
//!
//! `state.vscdb` is a SQLite database with one table, `ItemTable(key TEXT, value TEXT)`.
//! The key `history.recentlyOpenedPathsList` holds a JSON object:
//! ```json
//! {
//!   "entries": [
//!     { "folderUri": "file:///Users/user/project" },
//!     { "fileUri": "file:///Users/user/project/main.rs" },
//!     { "workspace": { "id": "abc123", "configPath": "file:///Users/user/ws.code-workspace" }, "label": "My WS" }
//!   ]
//! }
//! ```
//! Each entry has **one** of `folderUri`, `fileUri`, or a nested `workspace`
//! object (containing `id` and `configPath`); an optional `label`; and an
//! optional `remoteAuthority` (for SSH remotes, where the URI is
//! `vscode-remote://`).  The list is capped and rolling — entries age out
//! silently between syncs.
//!
//! Note: older VS Code documentation and some third-party sources describe a
//! flat `workspaceUri` field; that key does **not** appear in real
//! `state.vscdb` files.  The authoritative shape is the nested `workspace`
//! object confirmed by `microsoft/vscode` `workspaces.ts` `toStoreData`.
//!
//! ## Timestamps
//!
//! VS Code does **not** store an open timestamp in this list.  We record
//! `ts = collection time` (RFC3339 local) for every *new* path observed.
//! Paths are tracked in `.trove/vscode-sync.json` (`seen` set); a path
//! already in `seen` is skipped.  `guid = path` (the decoded `file://` URI).
//!
//! ## Multi-product support
//!
//! VS Code forks (Cursor, Codium, Windsurf …) share the same storage
//! layout under a different product directory.  We scan a small hardcoded list
//! of known product dirs under `~/Library/Application Support/`.  Remote-only
//! entries (`vscode-remote://` / no `file://` prefix) are skipped.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly, like the other always-on local collectors.
pub const VSCODE_SYNC_SECS: u64 = 3600;

/// Raw developer stream: one row per newly-seen path.
const DIR: &str = "developer/vscode";
/// Rebuildable seen-set cursor.
const SYNC_FILE: &str = ".trove/vscode-sync.json";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_vscode()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_paths > 0, || {
        format!(
            "vscode synced — {} new paths across {} products",
            s.new_paths, s.products_scanned
        )
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_vscode()?;
    let headline = if s.new_paths == 0 {
        format!(
            "VS Code is up to date — no new paths ({} products scanned)",
            s.products_scanned
        )
    } else {
        format!(
            "VS Code synced — {} new paths across {} products",
            s.new_paths, s.products_scanned
        )
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("new_paths", s.new_paths),
            ("products", s.products_scanned),
        ]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "vscode",
        name: "VS Code",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Recently opened folders, workspaces, and files from Visual Studio Code \
                      (and compatible forks like Cursor and Codium). \
                      Collected from VS Code's local state database — no network, no login.",
        domain: "developer",
        vault_path: "developer/vscode/",
        toggleable: true,
        setup: &[
            "Reads VS Code's local recently-opened list — nothing to install or connect.",
            "Works with VS Code, Cursor, Codium, and other VS Code forks installed under \
             ~/Library/Application Support/.",
        ],
        caveats: "Records only the rolling recently-opened list VS Code maintains internally. \
                  Entries that age out of the list before a sync are not captured. \
                  Remote workspaces (SSH remotes) are skipped — only local file:// paths are stored. \
                  Timestamps are collection-time, not actual open time.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(VSCODE_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// The raw row (this module's own shape — no contract).

/// One row in `developer/vscode/YYYY-MM.jsonl`: a newly-seen path.
/// `guid == path`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct VsCodeRow {
    /// Collection time (RFC3339 local).  VS Code does not record open timestamps.
    pub ts: String,
    /// Decoded absolute path (the `file://` URI without the scheme+authority).
    pub path: String,
    /// Entry type: `"folder"`, `"file"`, or `"workspace"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The product that yielded this entry (e.g. `"Code"`, `"Cursor"`).
    pub product: String,
    /// Optional human label from the entry (workspace name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `path` — the stable dedup key.
    pub guid: String,
}

// ---------------------------------------------------------------------------
// Sync state / cursor.

/// Persisted in `.trove/vscode-sync.json`.  `seen` maps `path → ts` of
/// first observation.  Rebuildable by rescanning the JSONL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VsCodeSyncState {
    /// RFC3339 local time of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// path → RFC3339 first-seen ts.
    #[serde(default)]
    pub seen: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Statistics.

/// Result of one scan pass.
#[derive(Debug, Clone, Default)]
pub struct VsCodeStats {
    /// VS Code product dirs found on this machine.
    pub products_scanned: u64,
    /// New paths written this pass.
    pub new_paths: u64,
}

// ---------------------------------------------------------------------------
// Product dirs.

/// Known VS Code-family product directory names under
/// `~/Library/Application Support/`.
const PRODUCT_DIRS: &[&str] = &[
    "Code",              // Visual Studio Code
    "Cursor",            // Cursor
    "VSCodium",          // VSCodium (open-source build)
    "Windsurf",          // Windsurf / Codeium
    "Code - OSS",        // VS Code OSS (Linux-style name)
    "Code - Insiders",   // VS Code Insiders
];

/// Candidate `state.vscdb` paths for all known products that are installed.
fn product_dbs(base_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for &product in PRODUCT_DIRS {
        let db = base_dir
            .join(product)
            .join("User/globalStorage/state.vscdb");
        if db.exists() {
            out.push((product.to_string(), db));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JSON schema for the ItemTable value.

/// Top-level JSON object stored in `history.recentlyOpenedPathsList`.
#[derive(Debug, Deserialize)]
struct RecentlyOpened {
    #[serde(default)]
    entries: Vec<RecentEntry>,
}

/// The nested workspace identifier object inside a workspace entry.
/// Shape: `{"id": "...", "configPath": "file:///...code-workspace"}`.
/// `id` is an opaque hash VS Code generates internally; we only need `configPath`.
#[derive(Debug, Deserialize)]
struct RecentWorkspace {
    /// The `file://` (or `vscode-remote://`) URI pointing to the
    /// `.code-workspace` file.  Decoded by `file_uri_to_path` just like
    /// `folderUri` / `fileUri`.
    #[serde(rename = "configPath")]
    config_path: String,
    /// Opaque hash; present in real data but not used by Trove.
    #[allow(dead_code)]
    #[serde(default)]
    id: String,
}

/// One entry in the recently-opened list.
///
/// VS Code serializes exactly one of three variants per entry:
/// - `{"folderUri": "file:///..."}` — a plain directory
/// - `{"fileUri": "file:///..."}` — a single file
/// - `{"workspace": {"id": "...", "configPath": "file:///...code-workspace"}}` — a
///   multi-root workspace.  The `workspace` key holds a nested object; there is
///   **no** flat `workspaceUri` key in real data.
#[derive(Debug, Deserialize)]
struct RecentEntry {
    #[serde(rename = "folderUri")]
    folder_uri: Option<String>,
    #[serde(rename = "fileUri")]
    file_uri: Option<String>,
    /// Nested workspace object — present only for `.code-workspace` entries.
    workspace: Option<RecentWorkspace>,
    label: Option<String>,
}

impl RecentEntry {
    /// Return `(kind, uri)`, or `None` for entries with no recognised URI field.
    fn uri_and_kind(&self) -> Option<(&str, &str)> {
        if let Some(u) = &self.folder_uri {
            Some(("folder", u.as_str()))
        } else if let Some(ws) = &self.workspace {
            // configPath is a `file://` URI; `file_uri_to_path` handles decode +
            // remote-URI rejection (vscode-remote:// configPaths are skipped).
            Some(("workspace", ws.config_path.as_str()))
        } else if let Some(u) = &self.file_uri {
            Some(("file", u.as_str()))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// URI → path.

/// Decode a `file://` URI to an absolute path.  Returns `None` for
/// remote URIs (`vscode-remote://`, `ssh-remote+...`, etc.) and for anything
/// that isn't a well-formed `file://` URI.
fn file_uri_to_path(uri: &str) -> Option<String> {
    let without_scheme = uri.strip_prefix("file://")?;
    // A plain `file:///absolute/path` → `/absolute/path`.
    // A `file://localhost/path` → `/path`.
    // We accept both; reject anything that still has a non-empty authority
    // that isn't `localhost` (i.e. remote URIs).
    // `file:///abs/path` → without_scheme = "/abs/path"  (common form)
    // `file://localhost/abs/path` → without_scheme = "localhost/abs/path"
    // Any other authority (non-empty, not localhost) → reject (remote URI).
    let path = if without_scheme.starts_with('/') {
        without_scheme
    } else if let Some(rest) = without_scheme.strip_prefix("localhost/") {
        // Restore the leading `/` that was part of the path segment.
        return {
            let full = format!("/{rest}");
            let decoded = percent_decode(&full);
            if decoded.is_empty() { None } else { Some(decoded) }
        };
    } else {
        // Non-empty, non-localhost authority → reject (remote/network URI).
        return None;
    };
    // Minimal percent-decode for `%20` (space) and similar.
    let decoded = percent_decode(path);
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

/// Decode `%XX` sequences.  VS Code's URIs primarily encode spaces and a few
/// other characters; this is a best-effort pass, not a full RFC 3986 decoder.
/// Invalid sequences are left verbatim.
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(hex) = h.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(hex as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// SQLite query (pure function — takes a temp-copy path).

/// Read the recently-opened entries from a copy of `state.vscdb`.
/// Returns an empty vec rather than erroring if the key is absent (older
/// VS Code versions or fresh installs may not have it yet).
fn read_recent_entries(tmp_db: &Path) -> Result<Vec<RecentEntry>> {
    let conn = rusqlite::Connection::open(tmp_db)
        .with_context(|| format!("opening {}", tmp_db.display()))?;
    // query_row returns QueryReturnedNoRows when the key is absent; .ok() maps
    // that to None, which we treat as an empty list (key not yet written by VS
    // Code).
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'history.recentlyOpenedPathsList'",
            [],
            |row| row.get(0),
        )
        .ok();
    let Some(json) = value else {
        return Ok(Vec::new());
    };
    let parsed: RecentlyOpened = serde_json::from_str(&json)
        .with_context(|| "parsing history.recentlyOpenedPathsList JSON")?;
    Ok(parsed.entries)
}

// ---------------------------------------------------------------------------
// Vault impl.

impl Vault {
    fn read_vscode_sync(&self) -> VsCodeSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_vscode_sync(&self, state: &VsCodeSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// One incremental pass: scan every known VS Code product database.
    pub fn collect_vscode(&self) -> Result<VsCodeStats> {
        let base = match dirs::home_dir() {
            Some(h) => h.join("Library/Application Support"),
            None => return Ok(VsCodeStats::default()),
        };
        self.collect_vscode_from(&base)
    }

    /// The pass itself, base-dir-injected for tests.
    pub(crate) fn collect_vscode_from(&self, base: &Path) -> Result<VsCodeStats> {
        let mut stats = VsCodeStats::default();
        let dbs = product_dbs(base);
        if dbs.is_empty() {
            return Ok(stats);
        }

        let mut state = self.read_vscode_sync();
        let now_ts = Local::now().to_rfc3339();
        let mut new_rows: Vec<VsCodeRow> = Vec::new();

        for (product, db_path) in &dbs {
            stats.products_scanned += 1;
            // Use both pid and a pointer-derived nonce so two concurrent test
            // threads that scan the same product name ("Code") don't race on
            // the same temp file.  Production runs one scan at a time (the
            // runner is single-threaded), so this is only relevant in tests.
            let nonce = (&state as *const _ as usize) ^ std::process::id() as usize;
            let stem = format!(
                "trove-vscode-{nonce}-{}",
                product.to_lowercase().replace(' ', "-")
            );
            match import_via_copy(db_path, &stem, |tmp| read_recent_entries(tmp)) {
                Err(e) => {
                    eprintln!("trove vscode: reading {db_path:?} failed: {e:#}");
                    continue; // one bad product never blocks the others
                }
                Ok(entries) => {
                    for entry in entries {
                        let Some((kind, uri)) = entry.uri_and_kind() else {
                            continue;
                        };
                        let Some(path) = file_uri_to_path(uri) else {
                            // Skip remote URIs and malformed entries.
                            continue;
                        };
                        if state.seen.contains_key(&path) {
                            continue; // already written on a previous pass
                        }
                        let row = VsCodeRow {
                            ts: now_ts.clone(),
                            guid: path.clone(),
                            path: path.clone(),
                            kind: kind.to_string(),
                            product: product.clone(),
                            label: entry.label.clone(),
                        };
                        state.seen.insert(path, now_ts.clone());
                        new_rows.push(row);
                    }
                }
            }
        }

        if new_rows.is_empty() {
            return Ok(stats);
        }

        // All new rows share `now_ts`, so they land in one month partition.
        let key = Partition::Month
            .key(&now_ts)
            .with_context(|| format!("unpartitionable ts {now_ts:?}"))?;
        let stream = self.stream(DIR, Partition::Month);
        let mut existing: Vec<VsCodeRow> = stream.read(key)?;
        let existing_guids: HashSet<String> = existing.iter().map(|r| r.guid.clone()).collect();
        for row in &new_rows {
            if !existing_guids.contains(&row.guid) {
                existing.push(row.clone());
                stats.new_paths += 1;
            }
        }
        self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &existing)?;

        state.updated = now_ts;
        self.write_vscode_sync(&state)?;
        Ok(stats)
    }

    /// All rows for one month (`YYYY-MM`), in file order.
    pub fn vscode_entries(&self, month: &str) -> Result<Vec<VsCodeRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-vscode-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Construct a minimal `state.vscdb` in `dir` with the given JSON value for
    /// `history.recentlyOpenedPathsList`.  Returns the db path.
    fn make_state_db(dir: &Path, json: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let db_path = dir.join("state.vscdb");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT NOT NULL, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES ('history.recentlyOpenedPathsList', ?1)",
            rusqlite::params![json],
        )
        .unwrap();
        db_path
    }

    /// Build a fake product dir tree under `base` for the named product.
    fn fake_product(base: &Path, product: &str, json: &str) {
        let dir = base.join(product).join("User/globalStorage");
        make_state_db(&dir, json);
    }

    // -----------------------------------------------------------------------

    #[test]
    fn file_uri_decoding() {
        assert_eq!(
            file_uri_to_path("file:///Users/david/project"),
            Some("/Users/david/project".to_string())
        );
        assert_eq!(
            file_uri_to_path("file:///Users/david/my%20project"),
            Some("/Users/david/my project".to_string())
        );
        assert_eq!(
            file_uri_to_path("file://localhost/Users/david/x"),
            Some("/Users/david/x".to_string())
        );
        // Remote URIs must be skipped.
        assert_eq!(file_uri_to_path("vscode-remote://ssh-remote+box/home/user"), None);
        assert_eq!(file_uri_to_path("ssh-remote+box/foo"), None);
        assert_eq!(file_uri_to_path(""), None);
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("/Users/david/my%20project"), "/Users/david/my project");
        assert_eq!(percent_decode("/no/encoding/here"), "/no/encoding/here");
        // Invalid escape left verbatim.
        assert_eq!(percent_decode("/bad/%ZZ/path"), "/bad/%ZZ/path");
    }

    #[test]
    fn reads_recently_opened_entries_from_db() {
        let dir = std::env::temp_dir()
            .join(format!("trove-vscode-readtest-{}", std::process::id()));
        let json = r#"{"entries":[
            {"folderUri":"file:///Users/dev/project"},
            {"fileUri":"file:///Users/dev/project/main.rs"},
            {"workspace":{"id":"abc123","configPath":"file:///Users/dev/ws.code-workspace"},"label":"Dev WS"}
        ]}"#;
        let db = make_state_db(&dir, json);
        let entries = read_recent_entries(&db).unwrap();
        assert_eq!(entries.len(), 3);
        let (k0, u0) = entries[0].uri_and_kind().unwrap();
        assert_eq!(k0, "folder");
        assert_eq!(u0, "file:///Users/dev/project");
        let (k1, _) = entries[1].uri_and_kind().unwrap();
        assert_eq!(k1, "file");
        let (k2, u2) = entries[2].uri_and_kind().unwrap();
        assert_eq!(k2, "workspace");
        assert_eq!(u2, "file:///Users/dev/ws.code-workspace");
        assert_eq!(entries[2].label.as_deref(), Some("Dev WS"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_key_yields_empty_list() {
        let dir = std::env::temp_dir()
            .join(format!("trove-vscode-emptykey-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("state.vscdb");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT NOT NULL, value TEXT);",
        )
        .unwrap();
        // No row inserted → key absent.
        let entries = read_recent_entries(&db_path).unwrap();
        assert!(entries.is_empty(), "absent key → empty list");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_new_paths_and_skips_seen() {
        let base = std::env::temp_dir()
            .join(format!("trove-vscode-collect-{}", std::process::id()));
        let v = temp_vault("collect");
        let json = r#"{"entries":[
            {"folderUri":"file:///Users/dev/alpha"},
            {"fileUri":"file:///Users/dev/alpha/lib.rs"}
        ]}"#;
        fake_product(&base, "Code", json);

        // First pass: both paths are new.
        let s1 = v.collect_vscode_from(&base).unwrap();
        assert_eq!(s1.products_scanned, 1);
        assert_eq!(s1.new_paths, 2);

        // Second pass: same DB → both paths already seen → 0 new.
        let s2 = v.collect_vscode_from(&base).unwrap();
        assert_eq!(s2.new_paths, 0, "idempotent: no duplicates on re-scan");

        // Verify rows land in the month partition.
        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.vscode_entries(&month).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.path == "/Users/dev/alpha" && r.kind == "folder"));
        assert!(rows.iter().any(|r| r.path == "/Users/dev/alpha/lib.rs" && r.kind == "file"));
        assert!(rows.iter().all(|r| r.product == "Code"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remote_uris_are_skipped() {
        let base = std::env::temp_dir()
            .join(format!("trove-vscode-remote-{}", std::process::id()));
        let v = temp_vault("remote");
        // Mix of local + remote entries.
        let json = r#"{"entries":[
            {"folderUri":"vscode-remote://ssh-remote+mybox/home/user/proj"},
            {"folderUri":"file:///Users/dev/local-only"}
        ]}"#;
        fake_product(&base, "Code", json);

        let s = v.collect_vscode_from(&base).unwrap();
        assert_eq!(s.new_paths, 1, "only the local file:// path written");
        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.vscode_entries(&month).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/Users/dev/local-only");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn multi_product_scan() {
        let base = std::env::temp_dir()
            .join(format!("trove-vscode-multi-{}", std::process::id()));
        let v = temp_vault("multi");
        fake_product(&base, "Code", r#"{"entries":[{"folderUri":"file:///code/a"}]}"#);
        fake_product(&base, "Cursor", r#"{"entries":[{"folderUri":"file:///cursor/b"}]}"#);

        let s = v.collect_vscode_from(&base).unwrap();
        assert_eq!(s.products_scanned, 2);
        assert_eq!(s.new_paths, 2);

        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.vscode_entries(&month).unwrap();
        assert_eq!(rows.len(), 2);
        let products: HashSet<&str> = rows.iter().map(|r| r.product.as_str()).collect();
        assert!(products.contains("Code"), "Code rows present: {products:?}");
        assert!(products.contains("Cursor"), "Cursor rows present: {products:?}");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn no_product_dirs_is_quiet_noop() {
        let base = std::env::temp_dir()
            .join(format!("trove-vscode-noproduct-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let v = temp_vault("noproduct");
        let s = v.collect_vscode_from(&base).unwrap();
        assert_eq!(s.products_scanned, 0);
        assert_eq!(s.new_paths, 0);
        assert!(!v.root().join(DIR).exists(), "no vault dir when nothing written");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn label_and_guid_fields_round_trip() {
        let base = std::env::temp_dir()
            .join(format!("trove-vscode-label-{}", std::process::id()));
        let v = temp_vault("label");
        let json = r#"{"entries":[
            {"workspace":{"id":"def456","configPath":"file:///Users/dev/my.code-workspace"},"label":"My Workspace"}
        ]}"#;
        fake_product(&base, "Code", json);

        let s = v.collect_vscode_from(&base).unwrap();
        assert_eq!(s.new_paths, 1);
        let month = Local::now().format("%Y-%m").to_string();
        let rows = v.vscode_entries(&month).unwrap();
        assert_eq!(rows[0].kind, "workspace");
        assert_eq!(rows[0].label.as_deref(), Some("My Workspace"));
        assert_eq!(rows[0].guid, rows[0].path, "guid == path");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sync_state_back_compat_deserializes() {
        // Empty object.
        let empty: VsCodeSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.seen.is_empty());
        // Old state that only wrote `updated`.
        let old: VsCodeSyncState =
            serde_json::from_str(r#"{"updated":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert!(old.seen.is_empty());
        assert_eq!(old.updated, "2026-06-01T00:00:00-07:00");
        // Round-trip.
        let mut s = VsCodeSyncState::default();
        s.seen.insert("/a/b".into(), "2026-06-10T09:00:00Z".into());
        let json = serde_json::to_string(&s).unwrap();
        let back: VsCodeSyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen.get("/a/b").map(|s| s.as_str()), Some("2026-06-10T09:00:00Z"));
    }
}
