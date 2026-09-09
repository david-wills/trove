//! GitHub Copilot Chat — periodic local sync of Copilot Chat sessions from
//! VS Code's `workspaceStorage` directories.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/github-copilot.md.
//!
//! A **Periodic** local-file collector (the [`crate::claude_code`] shape, but
//! reading VS Code's workspaceStorage layout instead of ~/.claude). VS Code
//! writes one file per session under
//! `<base>/User/workspaceStorage/<hash>/chatSessions/<session-id>.{jsonl,json}`.
//!
//! ## Two file formats
//!
//! - **JSONL mutation log (VS Code ≥1.109)**: append-only mutation log,
//!   one JSON line per mutation. `kind:0` lines carry the initial full-state
//!   snapshot; `kind:1` lines carry delta patches (`k` = changed keys,
//!   `v` = new values). We replay all lines to reconstruct the final state.
//! - **JSON snapshot (older VS Code)**: single flat JSON object with the
//!   same fields. Read directly.
//!
//! When both `.jsonl` and `.json` exist for the same stem, the `.jsonl`
//! takes precedence (same rule VS Code itself uses).
//!
//! ## Workspace → repo mapping
//!
//! Each hash directory contains a `workspace.json` with a `folder` field
//! (`file:///path/to/folder`). We decode the URI and use that as the
//! `workspace_path` in the session row.
//!
//! ## Privacy — metadata-only by default
//!
//! Session content (prompt text, assistant responses) is AI conversation
//! content and is **opt-in only**. By default we capture only:
//! `ts` (creation time), `workspace_path`, `model`, `request_count`,
//! `summary` (the session title when VS Code supplies one). The full
//! `requests[]` content is written only when the
//! `github-copilot-transcripts` sub-toggle is on.
//!
//! ## Cursor / incremental
//!
//! `.trove/github-copilot-sync.json` maps `session_id → last-seen file
//! mtime (unix ms)`. Growing `.jsonl` files are replayed from scratch on
//! each mtime advance (the full-replay strategy matches `claude_code.rs`
//! and is safe because the mutation log is bounded per-session).
//!
//! **`developer/` is raw-only** (taxonomy decision): no domain contract,
//! no spec_validation row — this module owns its row shape.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly, matching other always-on local collectors.
pub const GITHUB_COPILOT_SYNC_SECS: u64 = 3600;

/// Raw metadata stream: one row per session.
const DIR: &str = "developer/github-copilot";
/// Opt-in full-content sidecars directory.
const TRANSCRIPTS_DIR: &str = "developer/github-copilot/transcripts";
/// Rebuildable cursor: session_id → last-seen mtime (unix ms).
const SYNC_FILE: &str = ".trove/github-copilot-sync.json";
/// The sub-toggle id for full conversation content.
const TRANSCRIPTS_ID: &str = "github-copilot-transcripts";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_github_copilot()?;
    Ok(crate::registry::CollectOutcome::note_if(s.sessions > 0, || {
        let mut note = format!("github copilot synced — {} sessions", s.sessions);
        if s.transcripts > 0 {
            note.push_str(&format!(", {} transcripts", s.transcripts));
        }
        note
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_github_copilot()?;
    let headline = if s.sessions == 0 {
        "GitHub Copilot is up to date — no changed sessions".to_string()
    } else {
        format!("GitHub Copilot synced — {} sessions updated", s.sessions)
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("sessions", s.sessions),
            ("transcripts", s.transcripts),
        ]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn transcripts_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(TRANSCRIPTS_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. The default metadata
/// arm; the full-content sidecar is the [`TRANSCRIPTS_DEF`] opt-in.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "github-copilot",
        name: "GitHub Copilot",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Captures your GitHub Copilot Chat session history — timestamps, \
                      workspace context, model used, and request count — from VS Code's \
                      local workspace storage. Pure-local: no network, no GitHub login.",
        domain: "developer",
        vault_path: "developer/github-copilot/",
        toggleable: true,
        setup: &[
            "Reads the session files VS Code already writes under \
             ~/Library/Application\u{a0}Support/Code/User/workspaceStorage — nothing to install or connect.",
            "Works with VS Code ≥1.109 (.jsonl mutation-log format) and older versions \
             (.json snapshot format).",
            "Chat sessions can also be exported manually via the \
             \"Chat: Export Chat\u{2026}\" VS Code command and dropped into the import box.",
        ],
        caveats: "Only session metadata is stored by default — never prompt or response text. \
                  Turn on \"GitHub Copilot — full transcripts\" to also save conversation content. \
                  Sessions with no requests are still recorded (creation time + workspace).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(GITHUB_COPILOT_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

/// The opt-in transcript sidecar sub-toggle. Covered by `DEF`'s pass; has no
/// pass of its own (the `CoveredBy` shape means the main scanner does both).
pub static TRANSCRIPTS_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: TRANSCRIPTS_ID,
        name: "GitHub Copilot — full transcripts",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Additionally stores the full content of each Copilot Chat session — \
                      your prompts, model responses, and context references — as sidecar \
                      files. Off by default; the main GitHub Copilot card stores only \
                      session metadata without it.",
        domain: "developer",
        vault_path: "developer/github-copilot/transcripts/",
        toggleable: true,
        setup: &["Enable only if you want the complete conversation text saved, not just session metadata."],
        caveats: "Stores the complete conversation content — every prompt, model response, \
                  and context attachment. Opt-in for exactly that reason; leave it off to \
                  keep only metadata.",
    },
    behavior: Behavior::CoveredBy("github-copilot"),
    permission: None,
    last_data: Some(transcripts_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// The metadata row (raw, this module's own shape — no contract).

/// One row in `developer/github-copilot/YYYY-MM.jsonl`: a session's metadata.
/// No conversation content is written here by design.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SessionRow {
    /// Session UUID — the filename stem. `guid == session_id`.
    pub session_id: String,
    /// RFC3339 creation time (from `creationDate` epoch ms, local tz).
    pub ts: String,
    /// Decoded workspace folder path (from `workspace.json`, best-effort).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_path: String,
    /// Human name of the workspace (last path component).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_name: String,
    /// VS Code session version number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// Model identifier, e.g. `"copilot/claude-sonnet-4.6"` (from the last
    /// request that recorded one). Omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Number of request/response pairs in the session.
    pub request_count: u64,
    /// RFC3339 local time of the last message, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_ts: Option<String>,
    /// Auto-generated session title, when VS Code supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Where the session was opened: `"panel"`, `"editor"`, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// Result of one scan pass.
#[derive(Debug, Clone, Default)]
pub struct GitHubCopilotStats {
    pub sessions: u64,
    pub transcripts: u64,
}

// ---------------------------------------------------------------------------
// Paths.

/// `~/Library/Application Support/Code/User` — the VS Code user data root.
fn vscode_user_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
    })
}

/// Every `chatSessions` directory under `workspaceStorage`. Returns
/// `(hash, chatSessions_path, workspace_json_path)` triples, sorted by hash.
fn chat_session_dirs(user_dir: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let storage = user_dir.join("workspaceStorage");
    let Ok(hashes) = fs::read_dir(&storage) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf, PathBuf)> = Vec::new();
    for entry in hashes.flatten() {
        let hash_path = entry.path();
        if !hash_path.is_dir() {
            continue;
        }
        let hash = entry.file_name().to_string_lossy().into_owned();
        let chat_dir = hash_path.join("chatSessions");
        if !chat_dir.is_dir() {
            continue;
        }
        let workspace_json = hash_path.join("workspace.json");
        out.push((hash, chat_dir, workspace_json));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Read and decode the workspace folder path from `workspace.json`.
/// Returns the decoded local path, or an empty string on any error.
/// workspace.json format: `{"folder":"file:///absolute/path"}`
fn read_workspace_path(workspace_json: &Path) -> String {
    let Ok(body) = fs::read_to_string(workspace_json) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return String::new();
    };
    let raw = v
        .get("folder")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Decode "file:///path" or "file:///encoded%20path".
    let without_scheme = raw.strip_prefix("file://").unwrap_or(raw);
    // Percent-decode the path component.
    percent_decode(without_scheme)
}

/// Minimal percent-decoding for path characters (only what macOS/VS Code
/// encodes in folder URIs — spaces, parentheses, etc.).
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.as_bytes();
    while !bytes.is_empty() {
        if bytes[0] == b'%' && bytes.len() >= 3 {
            if let Ok(hi) = u8::from_str_radix(std::str::from_utf8(&bytes[1..2]).unwrap_or(""), 16) {
                if let Ok(lo) = u8::from_str_radix(std::str::from_utf8(&bytes[2..3]).unwrap_or(""), 16) {
                    out.push(char::from(hi << 4 | lo));
                    bytes = &bytes[3..];
                    continue;
                }
            }
        }
        out.push(char::from(bytes[0]));
        bytes = &bytes[1..];
    }
    out
}

/// The last path component (the human workspace name).
fn workspace_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Epoch milliseconds → RFC3339 local time. Returns `None` on invalid input.
fn epoch_ms_to_rfc3339(ms: i64) -> Option<String> {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.with_timezone(&Local).to_rfc3339())
}

/// File mtime in unix milliseconds, for the cursor.
fn file_mtime_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

// ---------------------------------------------------------------------------
// Session state reconstruction.

/// The mutable session state we build by replaying a JSONL mutation log (or
/// reading a JSON snapshot directly). Only the fields we actually use.
#[derive(Debug, Clone, Default)]
struct SessionState {
    session_id: Option<String>,
    creation_date_ms: Option<i64>,
    last_message_date_ms: Option<i64>,
    version: Option<u64>,
    initial_location: Option<String>,
    summary: Option<String>,
    requests: Vec<Value>,
}

impl SessionState {
    /// Merge one `v` object (from `kind:0` initial snapshot or `kind:1` patch)
    /// into self. Only the fields we care about are read.
    fn merge_value(&mut self, v: &Value) {
        if let Some(id) = v.get("sessionId").and_then(Value::as_str) {
            self.session_id = Some(id.to_string());
        }
        if let Some(ms) = v.get("creationDate").and_then(Value::as_i64) {
            self.creation_date_ms = Some(ms);
        }
        if let Some(ms) = v.get("lastMessageDate").and_then(Value::as_i64) {
            self.last_message_date_ms = Some(ms);
        }
        if let Some(ver) = v.get("version").and_then(Value::as_u64) {
            self.version = Some(ver);
        }
        if let Some(loc) = v.get("initialLocation").and_then(Value::as_str) {
            self.initial_location = Some(loc.to_string());
        }
        // session title: VS Code ≥1.109 serialises as `customTitle` (v3 schema);
        // older/exported formats may use `computedTitle`, `title`, or `summary`.
        // Priority: customTitle > computedTitle > title > summary (last non-empty wins
        // within a single merge call because we overwrite in that order).
        for key in &["summary", "title", "computedTitle", "customTitle"] {
            if let Some(t) = v.get(*key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                self.summary = Some(t.to_string());
            }
        }
        // requests: replace if present (a patch replaces the whole array).
        if let Some(Value::Array(reqs)) = v.get("requests") {
            self.requests = reqs.clone();
        }
    }
}

/// Parse the JSONL mutation-log format (VS Code ≥1.109).
///
/// Each line is one of:
/// - `{"kind":0,"v":{...full_session_state...}}` — initial snapshot.
/// - `{"kind":1,"k":["key1","key2",…],"v":{...patched_values...}}` — delta.
///
/// We replay all lines sequentially, building the final state.  Unknown
/// `kind` values are silently skipped (forward-compat).
fn parse_jsonl(body: &str) -> SessionState {
    let mut state = SessionState::default();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue; // malformed line — skip, never fatal
        };
        let kind = rec.get("kind").and_then(Value::as_u64).unwrap_or(99);
        match kind {
            0 => {
                // Full snapshot: merge the entire `v` object.
                if let Some(v) = rec.get("v") {
                    state.merge_value(v);
                }
            }
            1 => {
                // Delta patch: `k` names the keys that changed; `v` has new values.
                // For the fields we track, any relevant key in `v` is merged.
                if let Some(v) = rec.get("v") {
                    state.merge_value(v);
                }
            }
            _ => {} // future format — ignore
        }
    }
    state
}

/// Parse the legacy JSON snapshot format (older VS Code).
fn parse_json(body: &str) -> SessionState {
    let mut state = SessionState::default();
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        state.merge_value(&v);
    }
    state
}

/// Extract the `model` from a `requests` array: the model id recorded on the
/// last request that has one. VS Code records it as `request.model` or
/// `request.modelId`. Returns `None` if not found in any request.
fn extract_model(requests: &[Value]) -> Option<String> {
    requests.iter().rev().find_map(|r| {
        r.get("model")
            .or_else(|| r.get("modelId"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    })
}

/// Extract the last request's `timestamp` (epoch ms) from the requests array.
/// VS Code serialises it as `ISerializableChatRequestData.timestamp` (epoch ms).
fn extract_last_request_ts(requests: &[Value]) -> Option<i64> {
    requests.iter().rev().find_map(|r| r.get("timestamp").and_then(Value::as_i64))
}

/// Convert a `SessionState` into a `SessionRow`. Returns `None` if the state
/// has no usable creation date (we cannot partition the row without a `ts`).
fn state_to_row(state: &SessionState, session_id: &str, workspace_path: &str) -> Option<SessionRow> {
    let ts = epoch_ms_to_rfc3339(state.creation_date_ms?)?;
    // Prefer the top-level `lastMessageDate` (present in legacy .json files).
    // Fall back to the last request's `timestamp` field, which VS Code ≥1.109
    // serialises on each request but does NOT re-emit as a top-level field in
    // the .jsonl mutation-log format — so this covers the common modern case.
    let last_message_ts = state
        .last_message_date_ms
        .or_else(|| extract_last_request_ts(&state.requests))
        .and_then(epoch_ms_to_rfc3339);
    let model = extract_model(&state.requests);
    Some(SessionRow {
        session_id: session_id.to_string(),
        ts,
        workspace_path: workspace_path.to_string(),
        workspace_name: workspace_name(workspace_path),
        version: state.version,
        model,
        request_count: state.requests.len() as u64,
        last_message_ts,
        summary: state.summary.clone(),
        location: state.initial_location.clone(),
    })
}

// ---------------------------------------------------------------------------
// Cursor.

/// Incremental-sync state, persisted in `.trove/github-copilot-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitHubCopilotSyncState {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// session_id → newest file mtime processed (unix ms).
    #[serde(default)]
    pub mtimes: BTreeMap<String, i64>,
}

impl Vault {
    fn read_github_copilot_sync(&self) -> GitHubCopilotSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_github_copilot_sync(&self, state: &GitHubCopilotSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    // -----------------------------------------------------------------------
    // The scan.

    /// One incremental scan pass over VS Code's workspaceStorage.
    /// Silently a no-op when the user data dir is missing.
    pub fn collect_github_copilot(&self) -> Result<GitHubCopilotStats> {
        let Some(user_dir) = vscode_user_dir() else {
            return Ok(GitHubCopilotStats::default());
        };
        let want_transcripts = self.integration_enabled(TRANSCRIPTS_ID);
        self.collect_github_copilot_from(&user_dir, want_transcripts)
    }

    /// The pass itself, user-dir-injected for tests.
    pub(crate) fn collect_github_copilot_from(
        &self,
        user_dir: &Path,
        want_transcripts: bool,
    ) -> Result<GitHubCopilotStats> {
        let mut stats = GitHubCopilotStats::default();
        let chat_dirs = chat_session_dirs(user_dir);
        if chat_dirs.is_empty() {
            return Ok(stats);
        }
        let mut sync_state = self.read_github_copilot_sync();
        let mut changed = false;

        for (_hash, chat_dir, workspace_json) in chat_dirs {
            // Read the workspace path once per hash dir.
            let workspace_path = read_workspace_path(&workspace_json);

            // Enumerate session files in this chatSessions directory.
            let session_files = list_session_files(&chat_dir);
            for (session_id, file_path) in session_files {
                let Some(mtime) = file_mtime_ms(&file_path) else {
                    continue;
                };
                // mtime gate: only process sessions whose file has changed.
                if sync_state.mtimes.get(&session_id) == Some(&mtime) {
                    continue;
                }
                let Ok(body) = fs::read_to_string(&file_path) else {
                    continue; // unreadable — retry next pass
                };

                let is_jsonl = file_path.extension().and_then(|x| x.to_str()) == Some("jsonl");
                let state = if is_jsonl {
                    parse_jsonl(&body)
                } else {
                    parse_json(&body)
                };

                let Some(row) = state_to_row(&state, &session_id, &workspace_path) else {
                    // No valid creation date — record the mtime so we don't re-read it.
                    sync_state.mtimes.insert(session_id, mtime);
                    changed = true;
                    continue;
                };

                if let Err(e) = self.upsert_copilot_session_row(&row) {
                    eprintln!("trove github-copilot: upsert for {session_id} failed: {e:#}");
                    continue; // cursor untouched → retried next pass
                }

                // Opt-in only: write the full-content sidecar.
                if want_transcripts && !state.requests.is_empty() {
                    if let Err(e) = self.write_copilot_transcript(&session_id, &state.requests) {
                        eprintln!("trove github-copilot: transcript for {session_id} failed: {e:#}");
                        continue; // cursor untouched → retried next pass
                    }
                    stats.transcripts += 1;
                }

                sync_state.mtimes.insert(session_id, mtime);
                stats.sessions += 1;
                changed = true;
            }
        }

        if changed {
            sync_state.updated = Local::now().to_rfc3339();
            self.write_github_copilot_sync(&sync_state)?;
        }
        Ok(stats)
    }

    /// Upsert one [`SessionRow`] into `developer/github-copilot/YYYY-MM.jsonl`
    /// (keyed by the session's creation month). Replaces an existing row with
    /// the same `session_id` in place; appends when new. Rewrites the
    /// partition atomically.
    fn upsert_copilot_session_row(&self, row: &SessionRow) -> Result<()> {
        let key = Partition::Month
            .key(&row.ts)
            .with_context(|| {
                format!(
                    "github-copilot session {} has an unpartitionable ts {:?}",
                    row.session_id, row.ts
                )
            })?;
        let stream = self.stream(DIR, Partition::Month);
        let mut rows: Vec<SessionRow> = stream.read(key)?;
        match rows.iter_mut().find(|r| r.session_id == row.session_id) {
            Some(existing) => *existing = row.clone(),
            None => rows.push(row.clone()),
        }
        self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &rows)
    }

    /// Write the full requests array as a sidecar JSONL, one request per line.
    fn write_copilot_transcript(&self, session_id: &str, requests: &[Value]) -> Result<()> {
        self.write_snapshot(&format!("{TRANSCRIPTS_DIR}/{session_id}.jsonl"), requests)
    }

    // -----------------------------------------------------------------------
    // Reads.

    /// All session rows for one month (`YYYY-MM`), in file order.
    pub fn github_copilot_sessions(&self, month: &str) -> Result<Vec<SessionRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

/// List all session files in a `chatSessions` directory, returning
/// `(session_id, canonical_path)` pairs. When both `.jsonl` and `.json`
/// exist for the same stem, the `.jsonl` takes precedence (VS Code's own
/// convention). Sorted by session_id for deterministic passes.
fn list_session_files(chat_dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(chat_dir) else {
        return Vec::new();
    };
    // Collect all files by stem; jsonl wins over json for the same stem.
    let mut by_stem: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        if ext != "jsonl" && ext != "json" {
            continue;
        }
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        // .jsonl always wins; only insert .json if nothing better is there yet.
        if ext == "jsonl" {
            by_stem.insert(stem, path);
        } else {
            by_stem.entry(stem).or_insert(path);
        }
    }
    by_stem.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Partition;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ghcopilot-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a minimal VS Code workspaceStorage tree:
    ///   <user_dir>/workspaceStorage/<hash>/chatSessions/<session_id>.<ext>
    ///   <user_dir>/workspaceStorage/<hash>/workspace.json
    fn make_workspace(
        user_dir: &Path,
        hash: &str,
        folder_uri: &str,
    ) -> (PathBuf, PathBuf) {
        let hash_dir = user_dir.join("workspaceStorage").join(hash);
        let chat_dir = hash_dir.join("chatSessions");
        fs::create_dir_all(&chat_dir).unwrap();
        let ws = hash_dir.join("workspace.json");
        fs::write(&ws, format!("{{\"folder\":\"{folder_uri}\"}}")).unwrap();
        (chat_dir, ws)
    }

    fn write_session_file(chat_dir: &Path, session_id: &str, ext: &str, body: &str) -> PathBuf {
        let path = chat_dir.join(format!("{session_id}.{ext}"));
        fs::write(&path, body).unwrap();
        path
    }

    // -------------------------------------------------------------------------
    // Synthetic JSONL (mutation-log format, ≥1.109)
    //
    // All content is HAND-BUILT synthetic data. PRIVATE_* strings mark content
    // that must never appear in the metadata-only partition file.

    fn jsonl_session_body() -> String {
        // kind:0 — initial snapshot with creationDate, sessionId, empty requests.
        // 1781092800000 = 2026-06-10T12:00:00Z epoch ms.
        let kind0 = serde_json::json!({
            "kind": 0,
            "v": {
                "version": 3,
                "sessionId": "sess-jsonl",
                "creationDate": 1781092800000i64,  // 2026-06-10T12:00:00Z
                "initialLocation": "panel",
                "requests": []
            }
        });
        // kind:1 — delta: requests array populated + lastMessageDate.
        // The request carries PRIVATE_ content (conversation text).
        let kind1 = serde_json::json!({
            "kind": 1,
            "k": ["requests", "lastMessageDate"],
            "v": {
                "requests": [
                    {
                        "id": "req-1",
                        "model": "copilot/claude-sonnet-4.6",
                        "message": "PRIVATE_PROMPT_TEXT please refactor",
                        "response": "PRIVATE_RESPONSE_TEXT here is how"
                    }
                ],
                "lastMessageDate": 1781093100000i64  // 2026-06-10T12:05:00Z
            }
        });
        // kind:1 — delta: title added.
        // VS Code ≥1.109 serialises session titles as `customTitle` (v3 schema).
        // Using the real key here — not the fictional `"title"` key the parser
        // previously read, which caused the self-consistency trap.
        let kind1b = serde_json::json!({
            "kind": 1,
            "k": ["customTitle"],
            "v": { "customTitle": "Refactor the service module" }
        });
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&kind0).unwrap(),
            serde_json::to_string(&kind1).unwrap(),
            serde_json::to_string(&kind1b).unwrap(),
        )
    }

    // Synthetic JSON (legacy snapshot format)
    // 1781006400000 = 2026-06-09T12:00:00Z epoch ms
    fn json_session_body() -> String {
        serde_json::to_string(&serde_json::json!({
            "version": 3,
            "sessionId": "sess-json",
            "creationDate": 1781006400000i64,  // 2026-06-09T12:00:00Z
            "lastMessageDate": 1781006700000i64,
            "initialLocation": "panel",
            "requests": [
                {
                    "id": "req-a",
                    "model": "copilot/gpt-4o",
                    "message": "PRIVATE_OLD_PROMPT",
                    "response": "PRIVATE_OLD_RESPONSE"
                }
            ]
        })).unwrap()
    }

    // An empty session — no requests, no model.
    // 1781006400000 = 2026-06-09T12:00:00Z epoch ms (same as json_session_body for partition)
    fn empty_session_body() -> String {
        // Single JSONL line: kind:0 mutation-log snapshot.
        let obj = serde_json::json!({
            "kind": 0,
            "v": {
                "version": 3,
                "sessionId": "sess-empty",
                "creationDate": 1781006400000i64,  // 2026-06-09T12:00:00Z
                "initialLocation": "panel",
                "requests": []
            }
        });
        format!("{}\n", serde_json::to_string(&obj).unwrap())
    }

    // -------------------------------------------------------------------------
    // Unit tests for helpers.

    #[test]
    fn parse_jsonl_replays_mutations_correctly() {
        let body = jsonl_session_body();
        let state = parse_jsonl(&body);
        assert_eq!(state.session_id.as_deref(), Some("sess-jsonl"));
        assert_eq!(state.creation_date_ms, Some(1781092800000));
        assert_eq!(state.last_message_date_ms, Some(1781093100000));
        assert_eq!(state.requests.len(), 1);
        assert_eq!(state.version, Some(3));
        assert_eq!(state.initial_location.as_deref(), Some("panel"));
        assert_eq!(state.summary.as_deref(), Some("Refactor the service module"));
    }

    #[test]
    fn parse_json_flat_snapshot() {
        let body = json_session_body();
        let state = parse_json(&body);
        assert_eq!(state.session_id.as_deref(), Some("sess-json"));
        assert_eq!(state.creation_date_ms, Some(1781006400000)); // 2026-06-09T12:00:00Z
        assert_eq!(state.requests.len(), 1);
    }

    #[test]
    fn extract_model_returns_last_recorded() {
        let reqs = vec![
            serde_json::json!({"model": "copilot/gpt-4o"}),
            serde_json::json!({"model": "copilot/claude-sonnet-4.6"}),
        ];
        assert_eq!(extract_model(&reqs), Some("copilot/claude-sonnet-4.6".to_string()));
    }

    #[test]
    fn extract_model_accepts_model_id_field() {
        let reqs = vec![serde_json::json!({"modelId": "copilot/o1-preview"})];
        assert_eq!(extract_model(&reqs), Some("copilot/o1-preview".to_string()));
    }

    #[test]
    fn extract_model_returns_none_when_missing() {
        let reqs = vec![serde_json::json!({"id": "r1"})];
        assert_eq!(extract_model(&reqs), None);
    }

    #[test]
    fn state_to_row_maps_fields() {
        let state = parse_jsonl(&jsonl_session_body());
        let row = state_to_row(&state, "sess-jsonl", "/Users/dev/myproject").unwrap();
        assert_eq!(row.session_id, "sess-jsonl");
        assert_eq!(row.workspace_path, "/Users/dev/myproject");
        assert_eq!(row.workspace_name, "myproject");
        assert_eq!(row.request_count, 1);
        assert_eq!(row.model.as_deref(), Some("copilot/claude-sonnet-4.6"));
        assert_eq!(row.summary.as_deref(), Some("Refactor the service module"));
        assert_eq!(row.location.as_deref(), Some("panel"));
        // The ts is local time from 1781092800000 ms (2026-06-10T12:00:00Z).
        // Local timezone shifts the display time but the month stays 2026-06.
        assert!(row.ts.starts_with("2026-06-"), "ts: {}", row.ts);
        // Verify the Partition::Month key lands in 2026-06.
        let key = Partition::Month.key(&row.ts).unwrap();
        assert_eq!(key, "2026-06", "partition key: {key}");
    }

    #[test]
    fn state_to_row_returns_none_without_creation_date() {
        let state = SessionState {
            session_id: Some("s".into()),
            creation_date_ms: None,
            ..Default::default()
        };
        assert!(state_to_row(&state, "s", "/p").is_none());
    }

    #[test]
    fn workspace_path_decoded_from_file_uri() {
        let tmp = std::env::temp_dir().join(format!("trove-ghws-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let ws = tmp.join("workspace.json");

        // Plain path.
        fs::write(&ws, r#"{"folder":"file:///Users/dev/my-project"}"#).unwrap();
        assert_eq!(read_workspace_path(&ws), "/Users/dev/my-project");

        // Percent-encoded spaces.
        fs::write(&ws, r#"{"folder":"file:///Users/dev/my%20project"}"#).unwrap();
        assert_eq!(read_workspace_path(&ws), "/Users/dev/my project");

        // Encoded parentheses (VS Code URI convention).
        fs::write(&ws, r#"{"folder":"file:///Users/dev/2%29%20Areas"}"#).unwrap();
        assert_eq!(read_workspace_path(&ws), "/Users/dev/2) Areas");

        // Missing file.
        let bad = tmp.join("missing.json");
        assert_eq!(read_workspace_path(&bad), "");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn jsonl_wins_over_json_for_same_stem() {
        let tmp = std::env::temp_dir().join(format!("trove-ghfiles-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("abc.json"), "json content").unwrap();
        fs::write(tmp.join("abc.jsonl"), "jsonl content").unwrap();
        fs::write(tmp.join("def.json"), "other json").unwrap();

        let files = list_session_files(&tmp);
        let abc = files.iter().find(|(id, _)| id == "abc").unwrap();
        assert!(
            abc.1.extension().and_then(|x| x.to_str()) == Some("jsonl"),
            ".jsonl must win over .json for same stem"
        );
        assert_eq!(files.len(), 2, "abc (jsonl wins) + def = 2 entries");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tolerant_parse_skips_malformed_jsonl_lines() {
        let body = concat!(
            "{\"kind\":0,\"v\":{\"sessionId\":\"s\",\"creationDate\":1780056000000}}\n",
            "this is not json at all\n",
            "{\"kind\":99,\"v\":{\"unknown\":true}}\n",
            "{\"kind\":1,\"k\":[\"requests\"],\"v\":{\"requests\":[{\"model\":\"copilot/gpt-4o\"}]}}\n",
        );
        let state = parse_jsonl(body);
        assert_eq!(state.session_id.as_deref(), Some("s"));
        assert_eq!(state.requests.len(), 1, "kind:1 applied after malformed line");
    }

    // -------------------------------------------------------------------------
    // Integration (scan) tests.

    /// Returns the month partition key for the given epoch ms (local time).
    fn month_key_for_ms(ms: i64) -> String {
        let ts = epoch_ms_to_rfc3339(ms).unwrap();
        Partition::Month.key(&ts).unwrap().to_string()
    }

    #[test]
    fn scan_writes_metadata_only_by_default() {
        let v = temp_vault("default");
        let user_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-default", std::process::id()));
        let (chat_dir, _) = make_workspace(
            &user_dir,
            "hash01",
            "file:///Users/dev/myproject",
        );
        write_session_file(&chat_dir, "sess-jsonl", "jsonl", &jsonl_session_body());
        // The creation date is 1781092800000 ms = 2026-06-10T12:00:00Z.
        let month = month_key_for_ms(1781092800000);

        // Default run: transcripts opt-in OFF.
        let stats = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 0, "no transcripts when opt-in off");

        // Metadata row landed in the creation-month partition.
        let rows = v.github_copilot_sessions(&month).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.session_id, "sess-jsonl");
        assert_eq!(row.request_count, 1);
        assert_eq!(row.model.as_deref(), Some("copilot/claude-sonnet-4.6"));
        assert_eq!(row.workspace_name, "myproject");

        // CRITICAL: transcripts dir must not exist.
        assert!(
            !v.root().join("developer/github-copilot/transcripts").exists(),
            "transcripts/ must not exist when opt-in is off"
        );

        // CRITICAL privacy: the metadata partition file must NOT contain
        // conversation text.
        let partition_path = v.root().join(format!("developer/github-copilot/{month}.jsonl"));
        let partition = fs::read_to_string(&partition_path).unwrap();
        for leak in ["PRIVATE_PROMPT_TEXT", "PRIVATE_RESPONSE_TEXT"] {
            assert!(
                !partition.contains(leak),
                "metadata row leaked conversation content {leak:?}"
            );
        }
        // The workspace name IS allowed.
        assert!(partition.contains("myproject"));

        let _ = fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn opt_in_writes_full_transcript_sidecar() {
        let v = temp_vault("optin");
        let user_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-optin", std::process::id()));
        let (chat_dir, _) = make_workspace(&user_dir, "hash02", "file:///Users/dev/proj");
        write_session_file(&chat_dir, "sess-jsonl", "jsonl", &jsonl_session_body());
        let month = month_key_for_ms(1781092800000);

        let stats = v.collect_github_copilot_from(&user_dir, true).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 1);

        let sidecar = v
            .root()
            .join("developer/github-copilot/transcripts/sess-jsonl.jsonl");
        assert!(sidecar.exists(), "opt-in writes the sidecar");
        let body = fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("PRIVATE_PROMPT_TEXT"), "full fidelity in the sidecar");

        // Metadata partition still has NO conversation content.
        let partition_path = v.root().join(format!("developer/github-copilot/{month}.jsonl"));
        let content = fs::read_to_string(&partition_path).unwrap();
        assert!(!content.contains("PRIVATE_PROMPT_TEXT"));

        let _ = fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn legacy_json_format_is_read() {
        let v = temp_vault("legacyjson");
        let user_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-legacy", std::process::id()));
        let (chat_dir, _) = make_workspace(&user_dir, "hash03", "file:///Users/dev/old");
        write_session_file(&chat_dir, "sess-json", "json", &json_session_body());
        // 1781006400000 = 2026-06-09T12:00:00Z
        let month = month_key_for_ms(1781006400000);

        let stats = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(stats.sessions, 1);

        let rows = v.github_copilot_sessions(&month).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "sess-json");
        assert_eq!(rows[0].model.as_deref(), Some("copilot/gpt-4o"));

        let _ = fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn empty_session_is_recorded_without_model() {
        let v = temp_vault("empty");
        let user_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-empty", std::process::id()));
        let (chat_dir, _) = make_workspace(&user_dir, "hash04", "file:///Users/dev/empty");
        write_session_file(&chat_dir, "sess-empty", "jsonl", &empty_session_body());
        // 1781006400000 = 2026-06-09T12:00:00Z
        let month = month_key_for_ms(1781006400000);

        let stats = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(stats.sessions, 1, "empty session is still recorded");

        let rows = v.github_copilot_sessions(&month).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_count, 0);
        assert_eq!(rows[0].model, None, "no model when no requests");

        let _ = fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn mtime_gate_skips_unchanged_sessions() {
        let v = temp_vault("mtime");
        let user_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-mtime", std::process::id()));
        let (chat_dir, _) = make_workspace(&user_dir, "hash05", "file:///Users/dev/p");
        write_session_file(&chat_dir, "sess-jsonl", "jsonl", &jsonl_session_body());

        let first = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(first.sessions, 1);

        // Re-scan without changing the file → mtime gate fires, 0 sessions.
        let noop = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(noop.sessions, 0, "mtime gate: unchanged file skipped");

        let _ = fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn upsert_replaces_in_place_does_not_duplicate() {
        let v = temp_vault("upsert");
        let user_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-upsert", std::process::id()));
        let (chat_dir, _) = make_workspace(&user_dir, "hash06", "file:///Users/dev/u");

        let path_a = write_session_file(&chat_dir, "sess-a", "jsonl", &jsonl_session_body());
        // sess-b uses same epoch ms as jsonl_session_body but written to a jsonl one-liner
        // so both land in the same partition month.
        let sess_b_body = {
            let obj = serde_json::json!({
                "kind": 0,
                "v": {
                    "version": 3,
                    "sessionId": "sess-b",
                    "creationDate": 1781092800000i64,
                    "initialLocation": "panel",
                    "requests": []
                }
            });
            format!("{}\n", serde_json::to_string(&obj).unwrap())
        };
        write_session_file(&chat_dir, "sess-b", "jsonl", &sess_b_body);
        let month = month_key_for_ms(1781092800000);

        let first = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(first.sessions, 2);
        let rows = v.github_copilot_sessions(&month).unwrap();
        assert_eq!(rows.len(), 2, "two distinct sessions");

        // Grow sess-a: add a second request in a new kind:1 mutation.
        let extra_mutation = serde_json::json!({
            "kind": 1,
            "k": ["requests"],
            "v": {
                "requests": [
                    {"id": "req-1", "model": "copilot/claude-sonnet-4.6"},
                    {"id": "req-2", "model": "copilot/gpt-4o"}
                ]
            }
        });
        let grown = format!("{}{}\n", jsonl_session_body(), serde_json::to_string(&extra_mutation).unwrap());
        fs::write(&path_a, &grown).unwrap();
        // Bump mtime so the cursor sees it as changed.
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        {
            let f = fs::OpenOptions::new().write(true).open(&path_a).unwrap();
            f.set_modified(t).unwrap();
        }

        let second = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(second.sessions, 1, "only sess-a reprocessed");

        let rows = v.github_copilot_sessions(&month).unwrap();
        assert_eq!(rows.len(), 2, "upsert replaced, did not duplicate");
        let a = rows.iter().find(|r| r.session_id == "sess-a").unwrap();
        assert_eq!(a.request_count, 2, "sess-a now has 2 requests");
        // The latest model should be gpt-4o (extract_model takes the last request).
        assert_eq!(a.model.as_deref(), Some("copilot/gpt-4o"));

        let _ = fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn missing_user_dir_is_quiet_noop() {
        let v = temp_vault("missing");
        let fake_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-missing", std::process::id()));
        // Never created.
        let stats = v.collect_github_copilot_from(&fake_dir, false).unwrap();
        assert_eq!(stats.sessions, 0);
        assert!(!v.root().join("developer/github-copilot").exists());
    }

    #[test]
    fn multiple_workspaces_all_scanned() {
        let v = temp_vault("multi");
        let user_dir = std::env::temp_dir()
            .join(format!("trove-ghuser-{}-multi", std::process::id()));
        let (chat_a, _) = make_workspace(&user_dir, "aaa", "file:///Users/dev/proj-a");
        let (chat_b, _) = make_workspace(&user_dir, "bbb", "file:///Users/dev/proj-b");
        // Both sessions use the same creation epoch so they land in the same partition.
        let sess_b_body = {
            let obj = serde_json::json!({
                "kind": 0,
                "v": {
                    "version": 3,
                    "sessionId": "sess-b",
                    "creationDate": 1781092800000i64,
                    "initialLocation": "panel",
                    "requests": []
                }
            });
            format!("{}\n", serde_json::to_string(&obj).unwrap())
        };

        write_session_file(&chat_a, "sess-a", "jsonl", &jsonl_session_body());
        write_session_file(&chat_b, "sess-b", "jsonl", &sess_b_body);
        let month = month_key_for_ms(1781092800000);

        let stats = v.collect_github_copilot_from(&user_dir, false).unwrap();
        assert_eq!(stats.sessions, 2, "sessions from two workspaces");

        let rows = v.github_copilot_sessions(&month).unwrap();
        assert_eq!(rows.len(), 2);
        let ws_names: std::collections::HashSet<&str> = rows.iter().map(|r| r.workspace_name.as_str()).collect();
        assert!(ws_names.contains("proj-a"), "proj-a present: {ws_names:?}");
        assert!(ws_names.contains("proj-b"), "proj-b present: {ws_names:?}");

        let _ = fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn cursor_back_compat_empty_and_old_deserialize() {
        let empty: GitHubCopilotSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.mtimes.is_empty());
        assert_eq!(empty.updated, "");

        let old: GitHubCopilotSyncState =
            serde_json::from_str(r#"{"updated":"2026-01-01T00:00:00-08:00"}"#).unwrap();
        assert_eq!(old.updated, "2026-01-01T00:00:00-08:00");
        assert!(old.mtimes.is_empty());

        // Round-trips.
        let mut s = GitHubCopilotSyncState::default();
        s.mtimes.insert("sess-x".into(), 1780056000000);
        let json = serde_json::to_string(&s).unwrap();
        let back: GitHubCopilotSyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mtimes.get("sess-x"), Some(&1780056000000));
    }

    #[test]
    fn session_row_sparse_serde_back_compat() {
        // An "old" row with only the required fields still deserializes.
        let sparse: SessionRow = serde_json::from_str(
            r#"{"session_id":"abc","ts":"2026-06-10T12:00:00+00:00","request_count":0}"#,
        )
        .unwrap();
        assert_eq!(sparse.session_id, "abc");
        assert_eq!(sparse.request_count, 0);
        assert_eq!(sparse.model, None);
        assert_eq!(sparse.workspace_path, "");
        // Round-trips without the omitted optional fields.
        let json = serde_json::to_string(&sparse).unwrap();
        assert!(!json.contains("\"model\""), "absent model not serialized: {json}");
        assert!(!json.contains("\"summary\""), "absent summary not serialized: {json}");
    }
}
