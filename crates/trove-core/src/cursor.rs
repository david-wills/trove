//! Cursor — periodic local sync of Cursor IDE AI chat/agent session history.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/cursor.md.
//!
//! A **Periodic** local-file collector (the [`crate::claude_code`] /
//! [`crate::github_copilot`] shape). Cursor stores session data in VS Code's
//! SQLite machinery at two locations:
//!
//! - **Primary (v1 target):** `~/Library/Application Support/Cursor/User/
//!   globalStorage/state.vscdb` — a SQLite DB with a `cursorDiskKV` table
//!   holding `composerData:<id>` keys (session metadata as plain JSON).
//! - **Workspace-scoped:** `...workspaceStorage/<hash>/state.vscdb` — same
//!   table; holds per-workspace composer state.
//! - **Legacy chat DBs:** `~/.cursor/chats/*/*/store.db` (meta + blobs) —
//!   documented by the community (vibe-replay.com); may not exist in newer
//!   Cursor installs that use the state.vscdb path exclusively.
//! - **AI tracking:** `~/.cursor/ai-tracking/ai-code-tracking.db` —
//!   `conversation_summaries` and `ai_code_hashes` tables (newer feature,
//!   code attribution tracking); surfaced here as supplemental data.
//! - **Agent transcripts:** `~/.cursor/projects/*/agent-transcripts/*.jsonl`.
//!
//! All DB paths are **copy-then-read** (Cursor holds locks while running).
//!
//! ## Two layers, one privacy line
//!
//! - **Metadata stream (default):** `developer/cursor/YYYY-MM.jsonl`, one
//!   [`SessionRow`] per session. Carries: id, mode, model, ts, message_count
//!   — **no conversation content**.
//! - **Transcript sidecars (opt-in):** `developer/cursor/transcripts/` —
//!   full JSONL content, only when `cursor-transcripts` is on.
//!
//! ## Incremental cursor
//!
//! `.trove/cursor-sync.json` — maps `session_id → last-seen createdAt (unix
//! ms)` and a `kv_rowid_hwm` high-water mark per DB path for the KV table,
//! rebuildable from the vault files.
//!
//! **`developer/` is raw-only** (taxonomy decision): no domain contract, no
//! spec_validation row — this module owns its row shape.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly — same cadence as claude_code / github_copilot.
pub const CURSOR_SYNC_SECS: u64 = 3600;

const DIR: &str = "developer/cursor";
const TRANSCRIPTS_DIR: &str = "developer/cursor/transcripts";
const SYNC_FILE: &str = ".trove/cursor-sync.json";
const TRANSCRIPTS_ID: &str = "cursor-transcripts";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_cursor()?;
    Ok(crate::registry::CollectOutcome::note_if(s.sessions > 0, || {
        let mut note = format!("cursor synced — {} sessions", s.sessions);
        if s.transcripts > 0 {
            note.push_str(&format!(", {} transcripts", s.transcripts));
        }
        note
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_cursor()?;
    let headline = if s.sessions == 0 {
        "Cursor is up to date — no changed sessions".to_string()
    } else {
        format!("Cursor synced — {} sessions updated", s.sessions)
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

/// Registered in [`crate::integrations::INTEGRATIONS`]. The default metadata
/// arm; the full-transcript sidecar is [`TRANSCRIPTS_DEF`] (opt-in, covered
/// by this pass).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "cursor",
        name: "Cursor",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Captures your Cursor AI chat and agent session history — timestamps, \
                      mode (chat/agent), model used, and message count — from the editor's \
                      local storage. Pure-local: no network, no auth.",
        domain: "developer",
        vault_path: "developer/cursor/",
        toggleable: true,
        setup: &[
            "Reads the session files Cursor already writes under \
             ~/Library/Application\u{a0}Support/Cursor and ~/.cursor — nothing to install or connect.",
            "The chat database is locked while Cursor is running; the collector copies it \
             before reading, so it works safely with Cursor open.",
        ],
        caveats: "Only session metadata is stored by default — never prompt or response text. \
                  Turn on \u{201c}Cursor \u{2014} full transcripts\u{201d} to also save conversation \
                  content. Sessions with no messages are still recorded.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(CURSOR_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

fn transcripts_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(TRANSCRIPTS_DIR))
}

/// Opt-in sub-arm: full conversation content sidecars. Covered by [`DEF`]'s
/// pass; has no pass of its own.
pub static TRANSCRIPTS_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: TRANSCRIPTS_ID,
        name: "Cursor \u{2014} full transcripts",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Additionally stores the full text of every Cursor chat and agent session — \
                      your prompts, model responses, and context — as sidecar files. Off by \
                      default; the Cursor card stores only metadata without it.",
        domain: "developer",
        vault_path: "developer/cursor/transcripts/",
        toggleable: true,
        setup: &["Enable only if you want the complete conversation text saved, not just session metadata."],
        caveats: "Stores the complete conversation content — every prompt, response, and context \
                  attachment. Opt-in for exactly that reason; leave it off to keep only metadata.",
    },
    behavior: Behavior::CoveredBy("cursor"),
    permission: None,
    last_data: Some(transcripts_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Row shape (raw, this module's own — no domain contract).

/// One row in `developer/cursor/YYYY-MM.jsonl`. Metadata only; no content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SessionRow {
    /// Stable session UUID. `guid == session_id`.
    pub session_id: String,
    /// RFC3339 local time when the session was created.
    pub ts: String,
    /// "agent" | "chat" (from `unifiedMode`/`isAgentic`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Model identifier, e.g. `"claude-sonnet-4-5"` (from `modelConfig.modelName`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Total message count (bubbles in `fullConversationHeadersOnly`).
    pub message_count: u64,
    /// Whether this was an agentic (autonomous) session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_agentic: Option<bool>,
    /// Session title/name, when Cursor supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Source DB hint: "state_vscdb" | "store_db" | "ai_tracking".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Result of one scan pass.
#[derive(Debug, Clone, Default)]
pub struct CursorStats {
    pub sessions: u64,
    pub transcripts: u64,
    pub skipped: u64,
}

// ---------------------------------------------------------------------------
// Paths.

/// Primary global state.vscdb.
fn global_state_vscdb() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    })
}

/// All per-workspace state.vscdb files.
fn workspace_state_vscdb_files() -> Vec<PathBuf> {
    let base = match dirs::home_dir() {
        Some(h) => h.join("Library/Application Support/Cursor/User/workspaceStorage"),
        None => return Vec::new(),
    };
    let Ok(entries) = fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path().join("state.vscdb");
        if p.exists() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Legacy `~/.cursor/chats/*/*/store.db` files (may be absent on newer Cursor).
fn legacy_store_dbs() -> Vec<PathBuf> {
    let base = match dirs::home_dir() {
        Some(h) => h.join(".cursor/chats"),
        None => return Vec::new(),
    };
    let Ok(workspaces) = fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ws in workspaces.flatten() {
        let Ok(sessions) = fs::read_dir(ws.path()) else {
            continue;
        };
        for sess in sessions.flatten() {
            let p = sess.path().join("store.db");
            if p.exists() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// AI tracking DB: `~/.cursor/ai-tracking/ai-code-tracking.db`.
fn ai_tracking_db() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cursor/ai-tracking/ai-code-tracking.db"))
}

/// Agent transcript JSONL files at `~/.cursor/projects/*/agent-transcripts/*.jsonl`.
fn agent_transcript_files() -> Vec<(String, PathBuf)> {
    let base = match dirs::home_dir() {
        Some(h) => h.join(".cursor/projects"),
        None => return Vec::new(),
    };
    let Ok(projects) = fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for proj in projects.flatten() {
        let at_dir = proj.path().join("agent-transcripts");
        let Ok(files) = fs::read_dir(&at_dir) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) {
                out.push((stem, p));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// Parsers.

/// Convert a Unix milliseconds integer to an RFC3339 local-tz string.
fn ms_to_rfc3339(ms: i64) -> Option<String> {
    let secs = ms / 1000;
    let ns = ((ms % 1000) * 1_000_000) as u32;
    Local.timestamp_opt(secs, ns).single().map(|dt| dt.to_rfc3339())
}

/// Parse a `composerData` JSON value from `cursorDiskKV` into a [`SessionRow`].
/// Tolerant: unknown fields are ignored; missing optional fields are None.
pub(crate) fn parse_composer_data(id: &str, raw: &str) -> Option<SessionRow> {
    let v: Value = serde_json::from_str(raw).ok()?;

    // `createdAt` is unix ms (stored as a string in the JSON).
    let created_at_ms: i64 = v
        .get("createdAt")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse().ok())
        .or_else(|| v.get("createdAt").and_then(|x| x.as_i64()))?;
    let ts = ms_to_rfc3339(created_at_ms)?;

    // Mode: prefer `unifiedMode`, fall back to `isAgentic`.
    // `isAgentic` is a native JSON bool on current Cursor (≥v0.40); older builds
    // stored it as a string "True"/"False". Handle both.
    let mode = v
        .get("unifiedMode")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            let agentic = v
                .get("isAgentic")
                .and_then(|x| {
                    // Native bool (current schema).
                    x.as_bool()
                        // String fallback for older Cursor builds.
                        .or_else(|| x.as_str().map(|s| s.eq_ignore_ascii_case("true")))
                })
                .unwrap_or(false);
            if agentic { Some("agent".to_string()) } else { None }
        });

    let is_agentic = mode.as_deref() == Some("agent");

    // Model: from `modelConfig.modelName`.
    // `modelConfig` is a native JSON object on current Cursor (≥v0.40); older builds
    // stored it as a JSON-encoded string. Handle both.
    let model = v
        .get("modelConfig")
        .and_then(|mc| {
            // Native object (current schema).
            mc.as_object()
                .and_then(|o| o.get("modelName"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                // String-encoded fallback for older Cursor builds.
                .or_else(|| {
                    mc.as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .and_then(|obj| obj.get("modelName").and_then(|m| m.as_str()).map(|s| s.to_string()))
                })
        })
        .filter(|s| !s.is_empty() && s != "default");

    // Message count: from `fullConversationHeadersOnly`.
    // On current Cursor (≥v0.40) this is a native JSON array; older builds stored
    // it as a JSON-encoded string. Handle both.
    let message_count: u64 = v
        .get("fullConversationHeadersOnly")
        .and_then(|x| {
            // Native array (current schema).
            x.as_array()
                .map(|a| a.len() as u64)
                // String-encoded fallback for older Cursor builds.
                .or_else(|| {
                    x.as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .and_then(|arr| arr.as_array().map(|a| a.len() as u64))
                })
        })
        .unwrap_or(0);

    // Name: session name if set.
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(SessionRow {
        session_id: id.to_string(),
        ts,
        mode,
        model,
        message_count,
        is_agentic: Some(is_agentic),
        name,
        source: Some("state_vscdb".to_string()),
    })
}

/// Parse a legacy `store.db` meta table (community-documented schema).
/// Returns `None` on any schema mismatch (tolerant: schema may drift).
fn parse_store_db(conn: &Connection, db_path: &Path) -> Vec<SessionRow> {
    // The meta table has a single row with JSON values for each key.
    // community-documented fields: agentId, name, mode, lastUsedModel, createdAt.
    let mut stmt = match conn.prepare("SELECT key, value FROM meta") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut meta: BTreeMap<String, String> = BTreeMap::new();
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return Vec::new();
    };
    for row in rows.flatten() {
        meta.insert(row.0, row.1);
    }

    // Derive session_id from agentId or the parent directory name.
    let session_id = meta.get("agentId").cloned().unwrap_or_else(|| {
        db_path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string())
    });

    // createdAt: try as unix ms integer string or as RFC3339.
    let ts = meta
        .get("createdAt")
        .and_then(|s| {
            // Try parsing as epoch ms.
            s.parse::<i64>().ok().and_then(ms_to_rfc3339).or_else(|| {
                // Might already be an RFC3339 string.
                if s.contains('T') { Some(s.clone()) } else { None }
            })
        })
        .unwrap_or_else(|| Local::now().to_rfc3339());

    // Message count from the blobs table (best-effort).
    let message_count: u64 = conn
        .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))
        .unwrap_or(0) as u64;

    let mode = meta
        .get("mode")
        .cloned()
        .filter(|s| !s.is_empty());
    let model = meta
        .get("lastUsedModel")
        .cloned()
        .filter(|s| !s.is_empty());
    let name = meta
        .get("name")
        .cloned()
        .filter(|s| !s.is_empty());

    vec![SessionRow {
        session_id,
        ts,
        mode,
        model,
        message_count,
        is_agentic: None,
        name,
        source: Some("store_db".to_string()),
    }]
}

// ---------------------------------------------------------------------------
// Cursor state (persisted).

/// Incremental sync state, rebuildable from vault files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CursorSyncState {
    /// RFC3339 of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// session_id → `createdAt` epoch ms seen last pass. Used to detect new
    /// sessions (Cursor doesn't mutate existing composerData rows; a new
    /// composerId means a new session).
    #[serde(default)]
    pub seen: BTreeMap<String, i64>,
}

// ---------------------------------------------------------------------------
// Vault impl.

impl Vault {
    fn read_cursor_sync(&self) -> CursorSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_cursor_sync(&self, state: &CursorSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Top-level entry point for the periodic scan.
    pub fn collect_cursor(&self) -> Result<CursorStats> {
        let want_transcripts = self.integration_enabled(TRANSCRIPTS_ID);
        self.collect_cursor_from_paths(
            global_state_vscdb(),
            workspace_state_vscdb_files(),
            legacy_store_dbs(),
            ai_tracking_db(),
            agent_transcript_files(),
            want_transcripts,
        )
    }

    /// Injected-paths variant for tests.
    pub(crate) fn collect_cursor_from_paths(
        &self,
        global_vscdb: Option<PathBuf>,
        workspace_vscdbs: Vec<PathBuf>,
        store_dbs: Vec<PathBuf>,
        _ai_tracking: Option<PathBuf>, // reserved for future use
        transcript_files: Vec<(String, PathBuf)>,
        want_transcripts: bool,
    ) -> Result<CursorStats> {
        let mut stats = CursorStats::default();
        let mut state = self.read_cursor_sync();
        let mut changed = false;

        // --- 1. Scan state.vscdb files (global + workspace-scoped) ---
        let mut all_vscdbs: Vec<PathBuf> = Vec::new();
        if let Some(p) = global_vscdb {
            if p.exists() {
                all_vscdbs.push(p);
            }
        }
        for p in workspace_vscdbs {
            if p.exists() {
                all_vscdbs.push(p);
            }
        }

        for db_path in &all_vscdbs {
            let stem = format!(
                "trove-cursor-kv-{}-{}",
                std::process::id(),
                db_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .replace(['/', '\\', ' '], "-")
            );
            let rows = import_via_copy(db_path, &stem, |tmp| {
                self.scan_state_vscdb(tmp, &mut state, &mut changed)
            })
            .unwrap_or_default();
            stats.sessions += rows.len() as u64;
            for row in rows {
                if let Err(e) = self.upsert_cursor_row(&row) {
                    eprintln!("trove cursor: upsert {} failed: {e:#}", row.session_id);
                    stats.skipped += 1;
                }
            }
        }

        // --- 2. Legacy store.db files ---
        for db_path in &store_dbs {
            let parent_stem = db_path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stem = format!("trove-cursor-store-{}-{parent_stem}", std::process::id());
            let rows = import_via_copy(db_path, &stem, |tmp| -> Result<Vec<SessionRow>> {
                let conn = Connection::open(tmp)?;
                Ok(parse_store_db(&conn, db_path))
            })
            .unwrap_or_default();
            for row in rows {
                if state.seen.contains_key(&row.session_id) {
                    continue; // already imported
                }
                // Dedup by key presence; value is a placeholder.
                state.seen.insert(row.session_id.clone(), 0i64);
                changed = true;
                if let Err(e) = self.upsert_cursor_row(&row) {
                    eprintln!("trove cursor: store.db upsert {} failed: {e:#}", row.session_id);
                    stats.skipped += 1;
                } else {
                    stats.sessions += 1;
                }
            }
        }

        // --- 3. Agent transcript JSONL files ---
        for (session_id, path) in &transcript_files {
            if state.seen.contains_key(session_id) && !want_transcripts {
                continue;
            }
            let Ok(body) = fs::read_to_string(path) else {
                continue;
            };
            // Emit a minimal session row from the transcript (best-effort).
            let (ts, message_count) = parse_transcript_metadata(&body);
            if ts.is_none() && message_count == 0 {
                stats.skipped += 1;
                continue;
            }
            let ts = ts.unwrap_or_else(|| Local::now().to_rfc3339());
            let row = SessionRow {
                session_id: session_id.clone(),
                ts: ts.clone(),
                mode: Some("agent".to_string()),
                model: None,
                message_count,
                is_agentic: Some(true),
                name: None,
                source: Some("agent_transcript".to_string()),
            };
            if !state.seen.contains_key(session_id) {
                state.seen.insert(session_id.clone(), 0);
                changed = true;
                if let Err(e) = self.upsert_cursor_row(&row) {
                    eprintln!("trove cursor: transcript upsert {session_id} failed: {e:#}");
                    stats.skipped += 1;
                } else {
                    stats.sessions += 1;
                }
            }

            if want_transcripts {
                if let Err(e) = self.write_cursor_transcript(session_id, &body) {
                    eprintln!("trove cursor: transcript sidecar {session_id} failed: {e:#}");
                } else {
                    stats.transcripts += 1;
                }
            }
        }

        if changed {
            state.updated = Local::now().to_rfc3339();
            self.write_cursor_sync(&state)?;
        }
        Ok(stats)
    }

    /// Scan a state.vscdb copy for `composerData:*` entries, returning newly-seen rows.
    fn scan_state_vscdb(
        &self,
        tmp: &Path,
        state: &mut CursorSyncState,
        changed: &mut bool,
    ) -> Result<Vec<SessionRow>> {
        let conn = Connection::open(tmp).context("opening state.vscdb copy")?;

        // Check table exists — older workspace storage may only have ItemTable.
        let has_kv: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='cursorDiskKV'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_kv {
            return Ok(Vec::new());
        }

        let mut stmt = conn
            .prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'")
            .context("preparing cursorDiskKV query")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .context("querying cursorDiskKV")?
            .filter_map(|r| r.ok())
            .collect();

        let mut out = Vec::new();
        for (key, value) in rows {
            // key = "composerData:<uuid>"
            let id = key.trim_start_matches("composerData:");
            if id.is_empty() {
                continue;
            }
            let Some(row) = parse_composer_data(id, &value) else {
                continue;
            };
            // Dedup by session id. Cursor composerData rows are immutable once
            // created (new session → new composerId), so presence in the seen
            // set is sufficient. The i64 value is a placeholder (0) — dedup
            // relies on key presence, not the value.
            if state.seen.contains_key(id) {
                continue;
            }
            state.seen.insert(id.to_string(), 0i64);
            *changed = true;
            out.push(row);
        }
        Ok(out)
    }

    /// Upsert one [`SessionRow`] into `developer/cursor/YYYY-MM.jsonl`.
    fn upsert_cursor_row(&self, row: &SessionRow) -> Result<()> {
        let key = Partition::Month
            .key(&row.ts)
            .with_context(|| format!("cursor session {} has bad ts {:?}", row.session_id, row.ts))?;
        let stream = self.stream(DIR, Partition::Month);
        let mut rows: Vec<SessionRow> = stream.read(key)?;
        match rows.iter_mut().find(|r| r.session_id == row.session_id) {
            Some(existing) => *existing = row.clone(),
            None => rows.push(row.clone()),
        }
        self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &rows)
    }

    /// Write agent transcript sidecar.
    fn write_cursor_transcript(&self, session_id: &str, body: &str) -> Result<()> {
        self.write_snapshot(
            &format!("{TRANSCRIPTS_DIR}/{session_id}.jsonl"),
            &body
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect::<Vec<_>>(),
        )
    }

    // -------------------------------------------------------------------
    // Reads.

    /// All session rows for one month (`YYYY-MM`), in file order.
    pub fn cursor_sessions(&self, month: &str) -> Result<Vec<SessionRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

// ---------------------------------------------------------------------------
// Helpers.

/// Best-effort: extract a `ts` and message count from an agent transcript JSONL.
fn parse_transcript_metadata(body: &str) -> (Option<String>, u64) {
    let mut ts: Option<String> = None;
    let mut count = 0u64;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        count += 1;
        if ts.is_none() {
            if let Some(t) = v.get("timestamp").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                ts = Some(t.to_string());
            }
        }
    }
    (ts, count)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-cursor-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // parse_composer_data tests — fixture data matching the real on-disk schema.
    //
    // Real schema (verified from state.vscdb on this machine, 7/7 rows):
    //   createdAt   → native JSON int (unix ms), e.g. 1779815196792
    //   isAgentic   → native JSON bool, e.g. false
    //   fullConversationHeadersOnly → native JSON array, e.g. []
    //   modelConfig → native JSON object, e.g. {"modelName":"claude-sonnet-4-5"}
    //
    // The parser also accepts the older string-encoded form (as_str fallbacks) so
    // older Cursor builds remain supported; the legacy-string fixtures below lock
    // that fallback path.

    /// Current-schema composerData (native JSON types, current Cursor ≥v0.40).
    fn composer_agent_fixture() -> (&'static str, &'static str) {
        (
            "c4b378d8-0968-489a-85d7-db49c372c80c",
            r#"{
                "_v": "16",
                "composerId": "c4b378d8-0968-489a-85d7-db49c372c80c",
                "createdAt": 1748000000000,
                "unifiedMode": "agent",
                "forceMode": "edit",
                "isDraft": false,
                "isAgentic": true,
                "fullConversationHeadersOnly": [{"bubbleId":"b1","type":1},{"bubbleId":"b2","type":2}],
                "modelConfig": {"modelName": "claude-sonnet-4-5", "maxMode": false},
                "name": "Refactor auth module"
            }"#,
        )
    }

    /// Current-schema chat session (native JSON types).
    fn composer_chat_fixture() -> (&'static str, &'static str) {
        (
            "a7e3e0e1-0666-4b5c-a6a6-d27156397f70",
            r#"{
                "_v": "16",
                "composerId": "a7e3e0e1-0666-4b5c-a6a6-d27156397f70",
                "createdAt": 1748001000000,
                "unifiedMode": "chat",
                "forceMode": "chat",
                "isDraft": false,
                "isAgentic": false,
                "fullConversationHeadersOnly": [],
                "modelConfig": {"modelName": "default", "maxMode": false},
                "name": ""
            }"#,
        )
    }

    /// Legacy-schema fixture: older Cursor builds stored createdAt/isAgentic/
    /// fullConversationHeadersOnly/modelConfig as JSON-encoded strings. The
    /// parser's fallback paths must still handle these.
    fn composer_agent_fixture_legacy_strings() -> (&'static str, &'static str) {
        (
            "d1111111-1111-1111-1111-111111111111",
            r#"{
                "_v": "15",
                "composerId": "d1111111-1111-1111-1111-111111111111",
                "createdAt": "1748100000000",
                "isAgentic": "True",
                "fullConversationHeadersOnly": "[{\"bubbleId\":\"x1\",\"type\":1}]",
                "modelConfig": "{\"modelName\": \"gpt-4o\", \"maxMode\": false}",
                "name": "Legacy session"
            }"#,
        )
    }

    #[test]
    fn parse_agent_session() {
        let (id, raw) = composer_agent_fixture();
        let row = parse_composer_data(id, raw).unwrap();
        assert_eq!(row.session_id, id);
        assert_eq!(row.mode.as_deref(), Some("agent"));
        assert_eq!(row.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(row.message_count, 2);
        assert_eq!(row.is_agentic, Some(true));
        assert_eq!(row.name.as_deref(), Some("Refactor auth module"));
        assert_eq!(row.source.as_deref(), Some("state_vscdb"));
        // ts must be an RFC3339 string derived from 1748000000000 ms.
        assert!(row.ts.contains('T'), "ts should be RFC3339: {}", row.ts);
    }

    #[test]
    fn parse_chat_session_no_model_default() {
        let (id, raw) = composer_chat_fixture();
        let row = parse_composer_data(id, raw).unwrap();
        assert_eq!(row.mode.as_deref(), Some("chat"));
        // "default" model name is suppressed.
        assert_eq!(row.model, None, "modelName='default' should be omitted");
        assert_eq!(row.message_count, 0);
        assert_eq!(row.is_agentic, Some(false));
        // Empty name should be omitted.
        assert_eq!(row.name, None, "empty name should be omitted");
    }

    #[test]
    fn parse_legacy_string_encoded_fields() {
        // Older Cursor builds stored isAgentic/fullConversationHeadersOnly/modelConfig
        // as JSON-encoded strings. Parser fallback paths must still decode them.
        let (id, raw) = composer_agent_fixture_legacy_strings();
        let row = parse_composer_data(id, raw).unwrap();
        assert_eq!(row.mode.as_deref(), Some("agent"), "isAgentic string 'True' → mode=agent");
        assert_eq!(row.is_agentic, Some(true));
        assert_eq!(row.model.as_deref(), Some("gpt-4o"), "modelConfig string-encoded object");
        assert_eq!(row.message_count, 1, "fullConversationHeadersOnly string-encoded array");
        assert!(row.ts.contains('T'), "createdAt string-encoded ms → RFC3339");
    }

    #[test]
    fn parse_composer_data_missing_created_at_returns_none() {
        // Missing createdAt → None (can't timestamp the row).
        let raw = r#"{"composerId": "x", "unifiedMode": "chat"}"#;
        assert!(
            parse_composer_data("x", raw).is_none(),
            "missing createdAt should return None"
        );
    }

    #[test]
    fn parse_malformed_json_returns_none() {
        assert!(parse_composer_data("x", "not json at all").is_none());
    }

    // -----------------------------------------------------------------------
    // Scan round-trip tests (synthetic state.vscdb built with rusqlite).

    /// Build a synthetic state.vscdb under a temp dir and return its path.
    fn build_state_vscdb(name: &str, rows: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-cursor-db-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.vscdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        for (key, value) in rows {
            conn.execute(
                "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn scan_state_vscdb_writes_sessions() {
        let v = temp_vault("scan");
        let (id1, raw1) = composer_agent_fixture();
        let (id2, raw2) = composer_chat_fixture();
        let kv_rows = [
            (format!("composerData:{id1}"), raw1),
            (format!("composerData:{id2}"), raw2),
            // A non-composer key that must be ignored.
            ("composerVirtualRowHeights:_recentIds".to_string(), "[]"),
        ];
        let kv_refs: Vec<(&str, &str)> = kv_rows.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let db_path = build_state_vscdb("scan", &kv_refs);

        let stats = v
            .collect_cursor_from_paths(
                Some(db_path),
                vec![],
                vec![],
                None,
                vec![],
                false,
            )
            .unwrap();

        assert_eq!(stats.sessions, 2, "two composerData rows → two sessions");
        assert_eq!(stats.transcripts, 0);
        assert_eq!(stats.skipped, 0);

        // Both rows must land in the correct month partition.
        // 1748000000000 ms = 2025-05 (approximately).
        // Determine actual month from the parsed ts.
        let row = parse_composer_data(id1, raw1).unwrap();
        let month = &row.ts[..7]; // "YYYY-MM"
        let rows = v.cursor_sessions(month).unwrap();
        assert_eq!(rows.len(), 2, "both sessions in the same month partition");
        assert!(rows.iter().any(|r| r.session_id == id1));
        assert!(rows.iter().any(|r| r.session_id == id2));
    }

    #[test]
    fn scan_idempotent_no_double_insert() {
        let v = temp_vault("idem");
        let (id1, raw1) = composer_agent_fixture();
        let kv_rows = [(format!("composerData:{id1}"), raw1)];
        let kv_refs: Vec<(&str, &str)> = kv_rows.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let db_path = build_state_vscdb("idem", &kv_refs);

        // First pass: 1 session inserted.
        let s1 = v
            .collect_cursor_from_paths(
                Some(db_path.clone()),
                vec![],
                vec![],
                None,
                vec![],
                false,
            )
            .unwrap();
        assert_eq!(s1.sessions, 1);

        // Second pass: nothing changes (seen set has the entry).
        let s2 = v
            .collect_cursor_from_paths(
                Some(db_path),
                vec![],
                vec![],
                None,
                vec![],
                false,
            )
            .unwrap();
        assert_eq!(s2.sessions, 0, "second pass: already-seen session not re-inserted");

        // Still only one row in the vault.
        let row = parse_composer_data(id1, raw1).unwrap();
        let month = &row.ts[..7];
        assert_eq!(v.cursor_sessions(month).unwrap().len(), 1);
    }

    #[test]
    fn privacy_default_no_transcripts() {
        let v = temp_vault("priv");
        // Build a synthetic agent-transcript JSONL.
        let transcript_dir = std::env::temp_dir()
            .join(format!("trove-cursor-at-{}", std::process::id()));
        fs::create_dir_all(&transcript_dir).unwrap();
        let session_id = "test-agent-session-1";
        let transcript_path = transcript_dir.join(format!("{session_id}.jsonl"));
        let content = concat!(
            r#"{"type":"user","timestamp":"2025-05-20T10:00:00Z","message":"SECRET_PROMPT_TEXT"}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2025-05-20T10:00:05Z","message":"SECRET_RESPONSE"}"#,
            "\n"
        );
        fs::write(&transcript_path, content).unwrap();

        // Default pass (opt-in off): metadata row written, no transcript sidecar.
        let stats = v
            .collect_cursor_from_paths(
                None,
                vec![],
                vec![],
                None,
                vec![(session_id.to_string(), transcript_path.clone())],
                false, // want_transcripts = false
            )
            .unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 0, "no sidecars when opt-in is off");

        // Confirm the transcripts/ dir was never created.
        assert!(
            !v.root().join("developer/cursor/transcripts").exists(),
            "transcripts dir must not exist with opt-in off"
        );

        // Confirm the metadata row carries no content.
        let rows = v.cursor_sessions("2025-05").unwrap();
        assert_eq!(rows.len(), 1);
        let meta_json = serde_json::to_string(&rows[0]).unwrap();
        assert!(!meta_json.contains("SECRET_PROMPT_TEXT"), "no content in metadata row");
        assert!(!meta_json.contains("SECRET_RESPONSE"), "no content in metadata row");
    }

    #[test]
    fn opt_in_writes_transcript_sidecar() {
        let v = temp_vault("optin");
        let transcript_dir = std::env::temp_dir()
            .join(format!("trove-cursor-atopt-{}", std::process::id()));
        fs::create_dir_all(&transcript_dir).unwrap();
        let session_id = "test-agent-session-2";
        let transcript_path = transcript_dir.join(format!("{session_id}.jsonl"));
        let content = concat!(
            r#"{"type":"user","timestamp":"2025-05-21T08:00:00Z","message":"OPT_IN_PROMPT"}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2025-05-21T08:00:03Z","message":"OPT_IN_RESPONSE"}"#,
            "\n"
        );
        fs::write(&transcript_path, content).unwrap();

        let stats = v
            .collect_cursor_from_paths(
                None,
                vec![],
                vec![],
                None,
                vec![(session_id.to_string(), transcript_path)],
                true, // want_transcripts = true
            )
            .unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 1);

        let sidecar = v
            .root()
            .join(format!("developer/cursor/transcripts/{session_id}.jsonl"));
        assert!(sidecar.exists(), "sidecar written with opt-in on");
        let body = fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("OPT_IN_PROMPT"), "full fidelity in sidecar");
    }

    #[test]
    fn missing_cursor_dir_is_quiet_noop() {
        let v = temp_vault("missing");
        let stats = v
            .collect_cursor_from_paths(None, vec![], vec![], None, vec![], false)
            .unwrap();
        assert_eq!(stats.sessions, 0);
        assert!(!v.root().join("developer/cursor").exists());
    }

    #[test]
    fn sync_state_round_trips() {
        let empty: CursorSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.seen.is_empty());
        let mut s = CursorSyncState::default();
        s.seen.insert("abc".into(), 1748000000000);
        let json = serde_json::to_string(&s).unwrap();
        let back: CursorSyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen.get("abc"), Some(&1748000000000));
    }
}
