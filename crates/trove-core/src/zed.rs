//! Zed editor AI conversation/thread history — periodic local-file collector.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/zed.md.
//!
//! Zed stores AI-session data in two generations:
//!
//! - **Legacy (older Zed versions):** `~/.config/zed/conversations/*.json` —
//!   plain JSON per conversation, format confirmed from `legacy_thread.rs`.
//!   Fields: `summary`, `updated_at` (RFC3339), `messages: Vec<{id,role,text}>`.
//! - **Current (Zed 2025+):** `~/Library/Application Support/Zed/threads/threads.db`
//!   — SQLite with a `threads` table; data column is either plain JSON
//!   (`data_type="json"`) or zstd-compressed JSON (`data_type="zstd"`).
//!   Schema confirmed from Zed db.rs (GitHub). Key columns: `id`, `summary`,
//!   `updated_at`, `created_at`, `folder_paths`, `parent_id`, `data_type`,
//!   `data`. The decompressed JSON is a `DbThread` object with a `version`
//!   field (e.g. `"0.3.0"`) plus `title`, `messages`, `model`, token counts,
//!   and other fields.
//!
//! **`developer/` is raw-only** (taxonomy decision): no domain contract, no
//! spec_validation row — this module owns its row shape, matching the
//! established AI-sessions pattern from `claude_code.rs`.
//!
//! ## Two layers, one privacy line
//!
//! - **Metadata stream (default):** `developer/zed/YYYY-MM.jsonl`, one
//!   [`SessionRow`] per thread/conversation. Carries: id, summary, timestamps,
//!   message count, folder paths, model name — **no conversation content**.
//! - **Transcript sidecars (opt-in):** `developer/zed/transcripts/` — one
//!   file per thread with the raw `data` payload, only when `zed-transcripts`
//!   is on.
//!
//! ## Scan / cursor / upsert
//!
//! Cursor at `.trove/zed-sync.json`:
//! - For the legacy path: seen-set of conversation filenames.
//! - For threads.db: `row_hwm` (max rowid seen) + upsert on `id` for
//!   updated_at-driven re-processing (threads can be updated in place).
//!
//! The mtime strategy from `claude_code.rs` applies to legacy JSON files;
//! for the SQLite path we read `updated_at > last_seen_ts` and upsert.
//!
//! **Copy-then-read**: threads.db is copied to a temp path before opening,
//! following the `imessage.rs` pattern (Zed holds a write lock while running).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly, matching other always-on local collectors.
pub const ZED_SYNC_SECS: u64 = 3600;

/// Per-process call counter used to generate unique temp-file names for the
/// copy-then-open threads.db pattern. Monotonically incremented on every
/// `collect_zed_db` call so that concurrent test threads (which share a PID)
/// each get a distinct path.
static DB_COPY_CTR: AtomicU64 = AtomicU64::new(0);

const DIR: &str = "developer/zed";
const TRANSCRIPTS_DIR: &str = "developer/zed/transcripts";
const SYNC_FILE: &str = ".trove/zed-sync.json";
const TRANSCRIPTS_ID: &str = "zed-transcripts";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_zed()?;
    Ok(crate::registry::CollectOutcome::note_if(s.sessions > 0, || {
        let mut note = format!("zed synced — {} threads", s.sessions);
        if s.transcripts > 0 {
            note.push_str(&format!(", {} transcripts", s.transcripts));
        }
        note
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_zed()?;
    let headline = if s.sessions == 0 {
        "Zed is up to date — no new or updated threads".to_string()
    } else {
        format!("Zed synced — {} threads updated", s.sessions)
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

/// Registered in [`crate::integrations::INTEGRATIONS`]. Metadata arm; the
/// full-transcript sidecar is the [`TRANSCRIPTS_DEF`] opt-in.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "zed",
        name: "Zed",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Captures your Zed AI assistant thread history — timestamps, \
             workspace context, model, and message counts — from the local \
             SQLite database (threads.db) and any legacy conversation JSON files.",
        domain: "developer",
        vault_path: "developer/zed/",
        toggleable: true,
        setup: &[
            "Reads the Zed threads database from ~/Library/Application Support/Zed/threads/threads.db \
             and any legacy conversations from ~/.config/zed/conversations/.",
            "No connection or install required — Trove reads local files Zed already writes.",
        ],
        caveats: "Only session metadata (timestamps, message count, model, workspace folder) is \
                  stored by default — never the actual prompts or AI responses. \
                  Turn on \u{201C}Zed — full transcripts\u{201D} to also save conversation content. \
                  Requires Zed to be installed and used at least once.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(ZED_SYNC_SECS),
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

/// The opt-in full-transcript sub-arm. Covered by [`DEF`]'s pass, no pass
/// of its own. When on, the scan writes each thread's raw decompressed JSON
/// payload to `developer/zed/transcripts/<id>.jsonl`.
pub static TRANSCRIPTS_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: TRANSCRIPTS_ID,
        name: "Zed — full transcripts",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Additionally stores the full content of every Zed AI thread — your prompts, \
                      the model's responses, and any tool interactions — as a sidecar file. \
                      Off by default; the Zed card stores only metadata without it.",
        domain: "developer",
        vault_path: "developer/zed/transcripts/",
        toggleable: true,
        setup: &["Enable only if you want the complete conversation content saved, not just session metadata."],
        caveats: "Stores the complete conversation: every prompt, model response, and tool call. \
                  Opt-in for exactly that reason. Runs with the Zed collector's pass.",
    },
    behavior: Behavior::CoveredBy("zed"),
    permission: None,
    last_data: Some(transcripts_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Row shape (raw, developer/-only — no contract).

/// One row in `developer/zed/YYYY-MM.jsonl`: metadata for a Zed AI thread or
/// legacy conversation. **No conversation content** — that is opt-in only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SessionRow {
    /// Stable thread/conversation id. For threads.db this is the `id` column
    /// TEXT; for legacy JSON files it is the filename stem.
    pub id: String,
    /// Thread title / summary from the `summary` column or `summary` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// RFC3339 creation timestamp (from `created_at` or file mtime).
    pub created_at: String,
    /// RFC3339 last-updated timestamp (from `updated_at`).
    pub updated_at: String,
    /// Number of messages in the thread (user + assistant combined).
    #[serde(default)]
    pub message_count: u64,
    /// Workspace folder path(s) associated with the thread.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folder_paths: Vec<String>,
    /// Parent thread id, for subagent threads. Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Model name/id from the `model` field in the thread data, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Source: "threads_db" or "legacy_json".
    pub source: String,
}

/// Stats from one scan pass.
#[derive(Debug, Clone, Default)]
pub struct ZedStats {
    /// Threads/conversations written or updated.
    pub sessions: u64,
    /// Transcript sidecars written (0 unless the opt-in is on).
    pub transcripts: u64,
    /// Items skipped due to parse errors.
    pub skipped: u64,
}

// ---------------------------------------------------------------------------
// Paths.

/// macOS threads.db: `~/Library/Application Support/Zed/threads/threads.db`.
/// Linux fallback: `~/.local/share/zed/threads/threads.db`.
fn threads_db_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join("Library/Application Support/Zed/threads/threads.db"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::data_local_dir().map(|d| d.join("zed/threads/threads.db"))
    }
}

/// Legacy conversations dir: `~/.config/zed/conversations/`.
fn legacy_conversations_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/zed/conversations"))
}

/// File mtime in unix milliseconds, for the legacy cursor.
fn file_mtime_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

// ---------------------------------------------------------------------------
// Cursor.

/// Incremental-sync state for Zed. Persisted at `.trove/zed-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZedSyncState {
    /// RFC3339 of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Legacy JSON files: stem → last-seen file mtime (unix ms).
    #[serde(default)]
    pub legacy_mtimes: BTreeMap<String, i64>,
    /// threads.db: thread id → last-seen updated_at string (for upsert gating).
    #[serde(default)]
    pub db_updated_at: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// zstd decompression (pure-Rust, no C).

/// Decompress a zstd blob to bytes, using the pure-Rust `ruzstd` crate.
/// Returns `Err` only on genuine decompression failure.
fn zstd_decompress(compressed: &[u8]) -> Result<Vec<u8>> {
    use ruzstd::decoding::StreamingDecoder;
    use ruzstd::io::Read;
    let mut src: &[u8] = compressed;
    let mut decoder = StreamingDecoder::new(&mut src)
        .map_err(|e| anyhow::anyhow!("zstd decoder init: {e}"))?;
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| anyhow::anyhow!("zstd decompress: {e}"))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// threads.db parse helpers.

/// Extract a model name from a `DbThread`-shaped JSON `Value`.
/// The model object lives at `.model.provider_id` / `.model.model` /
/// `.model` (string) — Zed has changed this field over versions. We make a
/// best-effort extraction so a miss never fails the row.
fn extract_model(data: &Value) -> Option<String> {
    // Try `.model.model` first (current DbLanguageModel shape).
    if let Some(name) = data
        .get("model")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(name.to_string());
    }
    // Try `.model` as a plain string (legacy or simplified shape).
    if let Some(name) = data
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(name.to_string());
    }
    None
}

/// Count messages in a `DbThread` JSON `messages` array. Tolerant: a missing
/// or non-array field yields 0 rather than an error.
fn count_messages(data: &Value) -> u64 {
    data.get("messages")
        .and_then(Value::as_array)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

/// Parse the `folder_paths` TEXT column. Zed stores it as a JSON array of
/// strings or as a bare newline-separated list — tolerate both.
fn parse_folder_paths(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    // Try JSON array first.
    if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(raw) {
        return arr
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    // Fallback: newline-separated or comma-separated strings.
    raw.split(['\n', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Legacy JSON parse.

/// Parse a Zed legacy conversation JSON file (pre-threads.db era).
/// Format from Zed's `legacy_thread.rs`:
/// `{ "summary": "...", "updated_at": "...", "messages": [{id, role, text, ...}] }`
fn parse_legacy_conversation(body: &str, stem: &str) -> Option<ParsedSession> {
    let v: Value = serde_json::from_str(body).ok()?;
    let summary = v
        .get("summary")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let updated_at = v
        .get("updated_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)?; // no timestamp → skip
    // No `created_at` in the legacy format; use `updated_at` as fallback.
    let created_at = v
        .get("created_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| updated_at.clone());
    let message_count = v
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
        .unwrap_or(0);
    Some(ParsedSession {
        row: SessionRow {
            id: stem.to_string(),
            summary,
            created_at,
            updated_at,
            message_count,
            folder_paths: Vec::new(),
            parent_id: None,
            model: None,
            source: "legacy_json".to_string(),
        },
        content: v,
    })
}

/// A parsed thread with its metadata row and raw content value.
struct ParsedSession {
    row: SessionRow,
    /// The full parsed JSON (for opt-in transcript sidecar). For legacy files
    /// this is the whole conversation object; for threads.db rows this is the
    /// decompressed data JSON.
    content: Value,
}

// ---------------------------------------------------------------------------
// The vault impl.

impl Vault {
    fn read_zed_sync(&self) -> ZedSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_zed_sync(&self, state: &ZedSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Main entry point: scan both sources, collect stats.
    pub fn collect_zed(&self) -> Result<ZedStats> {
        let want_transcripts = self.integration_enabled(TRANSCRIPTS_ID);
        self.collect_zed_from(
            threads_db_path().as_deref(),
            legacy_conversations_dir().as_deref(),
            want_transcripts,
        )
    }

    /// Injected paths for testing.
    pub(crate) fn collect_zed_from(
        &self,
        db_path: Option<&Path>,
        legacy_dir: Option<&Path>,
        want_transcripts: bool,
    ) -> Result<ZedStats> {
        let mut stats = ZedStats::default();
        let mut state = self.read_zed_sync();
        let mut changed = false;

        // 1. Legacy JSON conversations.
        if let Some(dir) = legacy_dir {
            if let Ok(entries) = fs::read_dir(dir) {
                let mut files: Vec<(String, PathBuf)> = entries
                    .flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) == Some("json") {
                            let stem = p.file_stem()?.to_string_lossy().into_owned();
                            Some((stem, p))
                        } else {
                            None
                        }
                    })
                    .collect();
                files.sort_by(|a, b| a.0.cmp(&b.0));

                for (stem, path) in files {
                    let mtime = file_mtime_ms(&path).unwrap_or(0);
                    if state.legacy_mtimes.get(&stem) == Some(&mtime) {
                        continue; // unchanged
                    }
                    let Ok(body) = fs::read_to_string(&path) else {
                        continue;
                    };
                    let Some(parsed) = parse_legacy_conversation(&body, &stem) else {
                        state.legacy_mtimes.insert(stem, mtime);
                        changed = true;
                        stats.skipped += 1;
                        continue;
                    };
                    if let Err(e) = self.upsert_zed_row(&parsed.row) {
                        eprintln!("trove zed: upsert legacy {stem} failed: {e:#}");
                        continue;
                    }
                    if want_transcripts {
                        if let Err(e) =
                            self.write_zed_transcript(&parsed.row.id, &parsed.content)
                        {
                            eprintln!("trove zed: transcript {stem} failed: {e:#}");
                            continue;
                        }
                        stats.transcripts += 1;
                    }
                    state.legacy_mtimes.insert(stem, mtime);
                    stats.sessions += 1;
                    changed = true;
                }
            }
        }

        // 2. threads.db (SQLite, copy-then-open).
        if let Some(orig) = db_path {
            if orig.exists() {
                match self.collect_zed_db(orig, &mut state, want_transcripts) {
                    Ok(db_stats) => {
                        stats.sessions += db_stats.sessions;
                        stats.transcripts += db_stats.transcripts;
                        stats.skipped += db_stats.skipped;
                        if db_stats.sessions > 0 || db_stats.skipped > 0 {
                            changed = true;
                        }
                    }
                    Err(e) => {
                        eprintln!("trove zed: threads.db scan failed: {e:#}");
                    }
                }
            }
        }

        if changed {
            state.updated = Local::now().to_rfc3339();
            self.write_zed_sync(&state)?;
        }
        Ok(stats)
    }

    /// Copy threads.db to a temp file, open it with rusqlite, scan for new/
    /// updated rows, upsert their metadata rows into the vault.
    fn collect_zed_db(
        &self,
        orig: &Path,
        state: &mut ZedSyncState,
        want_transcripts: bool,
    ) -> Result<ZedStats> {
        use rusqlite::{Connection, OpenFlags};

        let mut stats = ZedStats::default();

        // Copy-then-open (Zed holds the write lock while running).
        // Use a per-call unique suffix (PID + atomic counter) so that
        // concurrent test threads — which share the same PID — each get a
        // distinct temp-file path and don't race on cleanup.
        let ctr = DB_COPY_CTR.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!(
            "trove-zed-threads-{}-{ctr}.db",
            std::process::id()
        ));
        fs::copy(orig, &tmp)
            .with_context(|| format!("copy threads.db to {}", tmp.display()))?;
        let _tmp_guard = DeferDrop(tmp.clone());

        let conn = Connection::open_with_flags(
            &tmp,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("open threads.db copy")?;

        // Confirm the table exists before querying (schema might be absent on
        // a brand-new / corrupted DB).
        let has_table: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='threads'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);

        if !has_table {
            // Remove temp file immediately when we know we'll return early.
            let _ = fs::remove_file(&tmp);
            return Ok(stats);
        }

        // Read all thread rows (id, summary, updated_at, created_at,
        // folder_paths, parent_id, data_type, data).
        let mut stmt = conn
            .prepare(
                "SELECT id, summary, updated_at, created_at, folder_paths, \
                        parent_id, data_type, data FROM threads ORDER BY updated_at ASC",
            )
            .context("prepare threads SELECT")?;

        let rows_iter = stmt.query_map([], |row| {
            Ok(DbRow {
                id: row.get::<_, String>(0)?,
                summary: row.get::<_, Option<String>>(1)?,
                updated_at: row.get::<_, String>(2)?,
                created_at: row.get::<_, Option<String>>(3)?,
                folder_paths_raw: row.get::<_, Option<String>>(4)?,
                parent_id: row.get::<_, Option<String>>(5)?,
                data_type: row.get::<_, String>(6)?,
                data: row.get::<_, Vec<u8>>(7)?,
            })
        })?;

        for row_result in rows_iter {
            let db_row = match row_result {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("trove zed: row read error: {e}");
                    stats.skipped += 1;
                    continue;
                }
            };

            // Skip rows that haven't changed since the last pass.
            if state
                .db_updated_at
                .get(&db_row.id)
                .is_some_and(|prev| prev == &db_row.updated_at)
            {
                continue;
            }

            // Decompress/parse the data blob into JSON.
            let json_bytes = match db_row.data_type.as_str() {
                "zstd" => match zstd_decompress(&db_row.data) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("trove zed: zstd decompress {}: {e:#}", db_row.id);
                        stats.skipped += 1;
                        continue;
                    }
                },
                _ => db_row.data.clone(), // "json" or unknown: treat as raw bytes
            };

            let data: Value = match serde_json::from_slice(&json_bytes) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("trove zed: JSON parse {}: {e}", db_row.id);
                    stats.skipped += 1;
                    continue;
                }
            };

            let folder_paths = db_row
                .folder_paths_raw
                .as_deref()
                .map(parse_folder_paths)
                .unwrap_or_default();
            let model = extract_model(&data);
            let message_count = count_messages(&data);
            let created_at = db_row
                .created_at
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| db_row.updated_at.clone());

            let row = SessionRow {
                id: db_row.id.clone(),
                summary: db_row
                    .summary
                    .filter(|s| !s.is_empty()),
                created_at,
                updated_at: db_row.updated_at.clone(),
                message_count,
                folder_paths,
                parent_id: db_row.parent_id.filter(|s| !s.is_empty()),
                model,
                source: "threads_db".to_string(),
            };

            if let Err(e) = self.upsert_zed_row(&row) {
                eprintln!("trove zed: upsert db {} failed: {e:#}", db_row.id);
                continue;
            }

            if want_transcripts {
                if let Err(e) = self.write_zed_transcript(&db_row.id, &data) {
                    eprintln!("trove zed: transcript db {} failed: {e:#}", db_row.id);
                    continue;
                }
                stats.transcripts += 1;
            }

            state
                .db_updated_at
                .insert(db_row.id, db_row.updated_at);
            stats.sessions += 1;
        }

        // _tmp_guard (DeferDrop) removes the temp file when it goes out of scope.
        Ok(stats)
    }

    /// Upsert one [`SessionRow`] into `developer/zed/YYYY-MM.jsonl`
    /// (keyed by the thread's `created_at` month, which is stable).
    fn upsert_zed_row(&self, row: &SessionRow) -> Result<()> {
        let key = Partition::Month
            .key(&row.created_at)
            .with_context(|| {
                format!(
                    "zed thread {} has unpartitionable created_at {:?}",
                    row.id, row.created_at
                )
            })?;
        let stream = self.stream(DIR, Partition::Month);
        let mut rows: Vec<SessionRow> = stream.read(key)?;
        // Dedup on (source, id) so legacy_json and threads_db rows sharing
        // the same id string cannot overwrite each other in a shared partition.
        match rows
            .iter_mut()
            .find(|r| r.source == row.source && r.id == row.id)
        {
            Some(existing) => *existing = row.clone(),
            None => rows.push(row.clone()),
        }
        self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &rows)
    }

    /// Write one thread's content JSON as a transcript sidecar. Only called
    /// when the `zed-transcripts` opt-in is on.
    fn write_zed_transcript(&self, id: &str, content: &Value) -> Result<()> {
        // Write as a single-line JSONL (one record per file — `.jsonl`
        // extension matches the JSONL writer used by write_snapshot).
        self.write_snapshot(
            &format!("{TRANSCRIPTS_DIR}/{id}.jsonl"),
            std::slice::from_ref(content),
        )
    }

    /// Read all session rows for one month (`YYYY-MM`).
    pub fn zed_sessions(&self, month: &str) -> Result<Vec<SessionRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

/// Raw row as read from threads.db.
struct DbRow {
    id: String,
    summary: Option<String>,
    updated_at: String,
    created_at: Option<String>,
    folder_paths_raw: Option<String>,
    parent_id: Option<String>,
    data_type: String,
    data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// scopeguard lite — a minimal defer helper so we don't add a dep.
// We only need the "remove temp file on drop" pattern; a tiny local impl is
// cleaner than pulling in the full `scopeguard` crate.

struct DeferDrop(PathBuf);
impl Drop for DeferDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-zed-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-zedtmp-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -----------------------------------------------------------------------
    // parse_folder_paths tests.

    #[test]
    fn folder_paths_parses_json_array() {
        let result = parse_folder_paths(r#"["/Users/dev/proj","/Users/dev/other"]"#);
        assert_eq!(result, vec!["/Users/dev/proj", "/Users/dev/other"]);
    }

    #[test]
    fn folder_paths_empty_string() {
        assert!(parse_folder_paths("").is_empty());
        assert!(parse_folder_paths("  ").is_empty());
    }

    #[test]
    fn folder_paths_single_path() {
        let result = parse_folder_paths("/Users/dev/proj");
        assert_eq!(result, vec!["/Users/dev/proj"]);
    }

    // -----------------------------------------------------------------------
    // extract_model tests.

    #[test]
    fn extract_model_nested_object() {
        let data = serde_json::json!({"model": {"model": "claude-opus-4-5", "provider_id": "anthropic"}});
        assert_eq!(extract_model(&data).as_deref(), Some("claude-opus-4-5"));
    }

    #[test]
    fn extract_model_string_field() {
        let data = serde_json::json!({"model": "gpt-4o"});
        assert_eq!(extract_model(&data).as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn extract_model_missing_returns_none() {
        let data = serde_json::json!({"title": "hello"});
        assert_eq!(extract_model(&data), None);
    }

    // -----------------------------------------------------------------------
    // count_messages tests.

    #[test]
    fn count_messages_basic() {
        let data = serde_json::json!({
            "messages": [
                {"role": "User", "text": "hi"},
                {"role": "Assistant", "text": "hello"}
            ]
        });
        assert_eq!(count_messages(&data), 2);
    }

    #[test]
    fn count_messages_missing_is_zero() {
        assert_eq!(count_messages(&serde_json::json!({})), 0);
    }

    // -----------------------------------------------------------------------
    // Legacy JSON parse tests.

    fn legacy_body() -> &'static str {
        r#"{
            "summary": "Refactor the parser module",
            "updated_at": "2026-05-10T14:30:00Z",
            "messages": [
                {"id": 1, "role": "User", "text": "PRIVATE_PROMPT_TEXT"},
                {"id": 2, "role": "Assistant", "text": "PRIVATE_RESPONSE_TEXT"}
            ]
        }"#
    }

    fn legacy_body_no_summary() -> &'static str {
        r#"{
            "summary": "",
            "updated_at": "2026-04-01T09:00:00Z",
            "messages": [
                {"id": 1, "role": "User", "text": "hello"}
            ]
        }"#
    }

    #[test]
    fn parse_legacy_fields() {
        let p = parse_legacy_conversation(legacy_body(), "conv-abc").unwrap();
        assert_eq!(p.row.id, "conv-abc");
        assert_eq!(p.row.summary.as_deref(), Some("Refactor the parser module"));
        assert_eq!(p.row.updated_at, "2026-05-10T14:30:00Z");
        // created_at falls back to updated_at when absent.
        assert_eq!(p.row.created_at, "2026-05-10T14:30:00Z");
        assert_eq!(p.row.message_count, 2);
        assert_eq!(p.row.source, "legacy_json");
        assert!(p.row.folder_paths.is_empty());
        assert!(p.row.parent_id.is_none());
        assert!(p.row.model.is_none());
    }

    #[test]
    fn parse_legacy_empty_summary_becomes_none() {
        let p = parse_legacy_conversation(legacy_body_no_summary(), "conv-bare").unwrap();
        assert_eq!(p.row.summary, None);
    }

    #[test]
    fn parse_legacy_invalid_json_returns_none() {
        assert!(parse_legacy_conversation("this is not json", "bad").is_none());
    }

    #[test]
    fn parse_legacy_missing_updated_at_returns_none() {
        let body = r#"{"summary": "hello", "messages": []}"#;
        assert!(parse_legacy_conversation(body, "no-ts").is_none());
    }

    // -----------------------------------------------------------------------
    // Full legacy-dir scan (via collect_zed_from).

    fn write_legacy(dir: &Path, stem: &str, body: &str) {
        fs::write(dir.join(format!("{stem}.json")), body).unwrap();
    }

    #[test]
    fn scan_legacy_writes_metadata_no_transcripts_by_default() {
        let v = temp_vault("legacy-scan");
        let legacy_dir = temp_dir("legacy-conv");

        write_legacy(&legacy_dir, "conv-abc", legacy_body());

        let stats = v.collect_zed_from(None, Some(&legacy_dir), false).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 0);
        assert_eq!(stats.skipped, 0);

        // Row is in the 2026-05 partition.
        let rows = v.zed_sessions("2026-05").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "conv-abc");
        assert_eq!(rows[0].message_count, 2);
        assert_eq!(rows[0].source, "legacy_json");

        // CRITICAL: no transcript file exists.
        assert!(
            !v.root().join("developer/zed/transcripts").exists(),
            "transcripts/ must not exist when opt-in is off"
        );

        // CRITICAL: metadata partition must NOT contain conversation content.
        let partition =
            fs::read_to_string(v.root().join("developer/zed/2026-05.jsonl")).unwrap();
        for leak in ["PRIVATE_PROMPT_TEXT", "PRIVATE_RESPONSE_TEXT"] {
            assert!(
                !partition.contains(leak),
                "metadata row leaked conversation content {leak:?}: {partition}"
            );
        }
        // Summary (metadata, allowed) is present.
        assert!(partition.contains("Refactor the parser module"));
    }

    #[test]
    fn scan_legacy_opt_in_writes_transcript() {
        let v = temp_vault("legacy-optin");
        let legacy_dir = temp_dir("legacy-optin-conv");
        write_legacy(&legacy_dir, "conv-abc", legacy_body());

        let stats = v.collect_zed_from(None, Some(&legacy_dir), true).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 1);

        // Transcript sidecar exists and contains full content.
        let sidecar = v.root().join("developer/zed/transcripts/conv-abc.jsonl");
        assert!(sidecar.exists(), "opt-in writes the transcript sidecar");
        let body = fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("PRIVATE_PROMPT_TEXT"), "full content in sidecar");
        assert!(body.contains("PRIVATE_RESPONSE_TEXT"));
    }

    #[test]
    fn scan_legacy_mtime_gate_skips_unchanged() {
        let v = temp_vault("legacy-mtime");
        let legacy_dir = temp_dir("legacy-mtime-conv");
        write_legacy(&legacy_dir, "conv-abc", legacy_body());

        let first = v.collect_zed_from(None, Some(&legacy_dir), false).unwrap();
        assert_eq!(first.sessions, 1);

        // Second pass without mtime change: nothing re-processed.
        let second = v.collect_zed_from(None, Some(&legacy_dir), false).unwrap();
        assert_eq!(second.sessions, 0, "mtime gate: unchanged files skipped");
    }

    #[test]
    fn scan_legacy_upserts_on_update() {
        let v = temp_vault("legacy-upsert");
        let legacy_dir = temp_dir("legacy-upsert-conv");
        // Write a second conversation in the same month.
        let other = r#"{"summary":"Other","updated_at":"2026-05-11T10:00:00Z","messages":[{"id":1,"role":"User","text":"q"}]}"#;
        write_legacy(&legacy_dir, "conv-abc", legacy_body());
        write_legacy(&legacy_dir, "conv-other", other);

        let first = v.collect_zed_from(None, Some(&legacy_dir), false).unwrap();
        assert_eq!(first.sessions, 2);
        assert_eq!(v.zed_sessions("2026-05").unwrap().len(), 2);

        // Modify conv-abc (bump mtime by rewriting).
        let updated = r#"{"summary":"Refactor the parser module","updated_at":"2026-05-10T15:00:00Z","messages":[{"id":1,"role":"User","text":"a"},{"id":2,"role":"Assistant","text":"b"},{"id":3,"role":"User","text":"c"}]}"#;
        write_legacy(&legacy_dir, "conv-abc", updated);
        // Touch mtime by re-writing the file (it has different content).
        // Introduce a small delay-free mtime difference by writing explicitly.
        let path = legacy_dir.join("conv-abc.json");
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(t).unwrap();

        let second = v.collect_zed_from(None, Some(&legacy_dir), false).unwrap();
        assert_eq!(second.sessions, 1, "only the changed file re-processed");

        // Upsert: still exactly 2 rows in the partition.
        let rows = v.zed_sessions("2026-05").unwrap();
        assert_eq!(rows.len(), 2, "upsert replaced, did not duplicate");
        let abc = rows.iter().find(|r| r.id == "conv-abc").unwrap();
        assert_eq!(abc.message_count, 3, "count updated from 2→3");
    }

    #[test]
    fn cursor_state_round_trips() {
        let mut state = ZedSyncState::default();
        state.legacy_mtimes.insert("conv-abc".into(), 12345678);
        state
            .db_updated_at
            .insert("thread-xyz".into(), "2026-06-01T09:00:00Z".to_string());

        let json = serde_json::to_string(&state).unwrap();
        let back: ZedSyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.legacy_mtimes.get("conv-abc"), Some(&12345678));
        assert_eq!(
            back.db_updated_at.get("thread-xyz").map(|s| s.as_str()),
            Some("2026-06-01T09:00:00Z")
        );

        // Old/empty cursor deserializes cleanly.
        let empty: ZedSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.legacy_mtimes.is_empty());
        assert!(empty.db_updated_at.is_empty());
    }

    #[test]
    fn session_row_serializes_without_optional_fields() {
        let row = SessionRow {
            id: "t1".into(),
            summary: None,
            created_at: "2026-06-01T09:00:00Z".into(),
            updated_at: "2026-06-01T09:05:00Z".into(),
            message_count: 0,
            folder_paths: Vec::new(),
            parent_id: None,
            model: None,
            source: "threads_db".into(),
        };
        let json = serde_json::to_string(&row).unwrap();
        // None/empty optionals must not appear in the serialized form.
        assert!(!json.contains("\"summary\""), "absent summary not serialized");
        assert!(!json.contains("\"folder_paths\""), "empty folder_paths not serialized");
        assert!(!json.contains("\"parent_id\""), "absent parent_id not serialized");
        assert!(!json.contains("\"model\""), "absent model not serialized");
        // Required fields must be present.
        assert!(json.contains("\"id\":\"t1\""));
        assert!(json.contains("\"source\":\"threads_db\""));
    }

    #[test]
    fn missing_legacy_dir_is_quiet_noop() {
        let v = temp_vault("no-dir");
        let missing = std::env::temp_dir().join(format!(
            "trove-zed-missing-{}-absent",
            std::process::id()
        ));
        // Never created; must silently succeed.
        let stats = v.collect_zed_from(None, Some(&missing), false).unwrap();
        assert_eq!(stats.sessions, 0);
        assert!(!v.root().join("developer/zed").exists());
    }

    // -----------------------------------------------------------------------
    // threads.db SQLite scan tests (defect 1 coverage).

    /// Build a temporary threads.db with the real Zed schema and insert one
    /// `data_type="json"` row and one `data_type="zstd"` row.
    fn make_threads_db(path: &std::path::Path) {
        use rusqlite::Connection;
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id           TEXT NOT NULL PRIMARY KEY,
                summary      TEXT,
                updated_at   TEXT NOT NULL,
                created_at   TEXT,
                folder_paths TEXT,
                parent_id    TEXT,
                data_type    TEXT NOT NULL,
                data         BLOB NOT NULL
            );",
        )
        .unwrap();

        // Row 1: plain-JSON data_type.
        let json_data = serde_json::json!({
            "version": "0.3.0",
            "messages": [
                {"role": "User", "text": "Hello"},
                {"role": "Assistant", "text": "Hi there"}
            ],
            "model": {"model": "claude-opus-4-5", "provider_id": "anthropic"}
        });
        let json_bytes = serde_json::to_vec(&json_data).unwrap();
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, created_at, folder_paths, parent_id, data_type, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "thread-json-001",
                "A plain-JSON thread",
                "2026-06-01T10:00:00Z",
                "2026-06-01T09:00:00Z",
                r#"["/Users/dev/project"]"#,
                rusqlite::types::Null,
                "json",
                json_bytes,
            ],
        )
        .unwrap();

        // Row 2: zstd-compressed data_type.
        let zstd_data = serde_json::json!({
            "version": "0.3.0",
            "messages": [
                {"role": "User", "text": "What is Rust?"},
                {"role": "Assistant", "text": "A systems language."},
                {"role": "User", "text": "Thanks!"}
            ],
            "model": {"model": "gpt-4o", "provider_id": "openai"}
        });
        let zstd_src = serde_json::to_vec(&zstd_data).unwrap();
        let zstd_bytes = {
            use ruzstd::encoding::{compress_to_vec, CompressionLevel};
            compress_to_vec(zstd_src.as_slice(), CompressionLevel::Fastest)
        };
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, created_at, folder_paths, parent_id, data_type, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "thread-zstd-002",
                "A zstd-compressed thread",
                "2026-06-02T11:00:00Z",
                "2026-06-02T10:00:00Z",
                rusqlite::types::Null,
                rusqlite::types::Null,
                "zstd",
                zstd_bytes,
            ],
        )
        .unwrap();
    }

    #[test]
    fn scan_db_json_row_parsed_correctly() {
        let v = temp_vault("db-json");
        let db_dir = temp_dir("db-json-dir");
        let db_path = db_dir.join("threads.db");
        make_threads_db(&db_path);

        // Only collect the JSON row (scan with no legacy dir).
        let stats = v.collect_zed_from(Some(&db_path), None, false).unwrap();
        assert_eq!(stats.sessions, 2, "both db rows counted");
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.transcripts, 0, "transcripts off by default");

        // Check the JSON row in the 2026-06 partition.
        let rows = v.zed_sessions("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        let json_row = rows.iter().find(|r| r.id == "thread-json-001").unwrap();
        assert_eq!(json_row.source, "threads_db");
        assert_eq!(json_row.summary.as_deref(), Some("A plain-JSON thread"));
        assert_eq!(json_row.updated_at, "2026-06-01T10:00:00Z");
        assert_eq!(json_row.created_at, "2026-06-01T09:00:00Z");
        assert_eq!(json_row.message_count, 2);
        assert_eq!(json_row.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(json_row.folder_paths, vec!["/Users/dev/project"]);
        assert!(json_row.parent_id.is_none());
    }

    #[test]
    fn scan_db_zstd_row_decompresses_and_parses() {
        let v = temp_vault("db-zstd");
        let db_dir = temp_dir("db-zstd-dir");
        let db_path = db_dir.join("threads.db");
        make_threads_db(&db_path);

        let stats = v.collect_zed_from(Some(&db_path), None, false).unwrap();
        assert_eq!(stats.sessions, 2);
        assert_eq!(stats.skipped, 0, "zstd row must not be counted as skipped");

        let rows = v.zed_sessions("2026-06").unwrap();
        let zstd_row = rows.iter().find(|r| r.id == "thread-zstd-002").unwrap();
        assert_eq!(zstd_row.source, "threads_db");
        assert_eq!(zstd_row.summary.as_deref(), Some("A zstd-compressed thread"));
        assert_eq!(zstd_row.message_count, 3, "3 messages in zstd thread");
        assert_eq!(zstd_row.model.as_deref(), Some("gpt-4o"));
        assert!(zstd_row.folder_paths.is_empty());
    }

    #[test]
    fn scan_db_opt_in_writes_transcript_sidecar() {
        let v = temp_vault("db-transcript");
        let db_dir = temp_dir("db-transcript-dir");
        let db_path = db_dir.join("threads.db");
        make_threads_db(&db_path);

        let stats = v.collect_zed_from(Some(&db_path), None, true).unwrap();
        assert_eq!(stats.transcripts, 2, "opt-in writes sidecars for both rows");

        let s1 = v
            .root()
            .join("developer/zed/transcripts/thread-json-001.jsonl");
        let s2 = v
            .root()
            .join("developer/zed/transcripts/thread-zstd-002.jsonl");
        assert!(s1.exists(), "JSON-row transcript sidecar exists");
        assert!(s2.exists(), "zstd-row transcript sidecar exists");
        let body = fs::read_to_string(&s2).unwrap();
        assert!(body.contains("What is Rust?"), "sidecar has decompressed content");
    }

    #[test]
    fn scan_db_updated_at_cursor_gates_reprocessing() {
        let v = temp_vault("db-cursor");
        let db_dir = temp_dir("db-cursor-dir");
        let db_path = db_dir.join("threads.db");
        make_threads_db(&db_path);

        let first = v.collect_zed_from(Some(&db_path), None, false).unwrap();
        assert_eq!(first.sessions, 2, "first pass processes both rows");

        // Second pass with unchanged updated_at — cursor gates both rows.
        let second = v.collect_zed_from(Some(&db_path), None, false).unwrap();
        assert_eq!(second.sessions, 0, "cursor gate: unchanged rows skipped");
        assert_eq!(second.skipped, 0);

        // Simulate an in-place update: bump updated_at on the JSON row.
        {
            use rusqlite::Connection;
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE threads SET updated_at='2026-06-01T12:00:00Z', data=data \
                 WHERE id='thread-json-001'",
                [],
            )
            .unwrap();
        }
        let third = v.collect_zed_from(Some(&db_path), None, false).unwrap();
        assert_eq!(third.sessions, 1, "only the updated row re-processed");

        // Upsert must replace, not duplicate.
        let rows = v.zed_sessions("2026-06").unwrap();
        assert_eq!(rows.len(), 2, "upsert replaced, did not duplicate");
        let updated = rows.iter().find(|r| r.id == "thread-json-001").unwrap();
        assert_eq!(updated.updated_at, "2026-06-01T12:00:00Z");
    }

    #[test]
    fn upsert_cross_source_no_collision() {
        // A legacy_json row and a threads_db row with the same `id` in the same
        // month must NOT overwrite each other (dedup is on (source, id)).
        let v = temp_vault("cross-source");
        let legacy_dir = temp_dir("cross-source-legacy");
        let db_dir = temp_dir("cross-source-db");
        let db_path = db_dir.join("threads.db");

        // Write a legacy file whose stem matches the threads_db id.
        let shared_id = "thread-json-001";
        let legacy_body = format!(
            r#"{{"summary":"legacy version","updated_at":"2026-06-01T08:00:00Z","messages":[{{"id":1,"role":"User","text":"legacy"}}]}}"#
        );
        fs::write(
            legacy_dir.join(format!("{shared_id}.json")),
            legacy_body,
        )
        .unwrap();

        // Build the threads.db with the same id.
        make_threads_db(&db_path);

        // Collect both sources in one pass.
        let stats = v
            .collect_zed_from(Some(&db_path), Some(&legacy_dir), false)
            .unwrap();
        // 1 legacy + 2 db = 3 sessions (no silent override).
        assert_eq!(stats.sessions, 3, "legacy + db rows all written");

        let rows = v.zed_sessions("2026-06").unwrap();
        assert_eq!(rows.len(), 3, "3 distinct rows (source+id dedup)");
        let legacy = rows
            .iter()
            .find(|r| r.source == "legacy_json" && r.id == shared_id)
            .unwrap();
        let db = rows
            .iter()
            .find(|r| r.source == "threads_db" && r.id == shared_id)
            .unwrap();
        assert_eq!(legacy.summary.as_deref(), Some("legacy version"));
        assert_eq!(db.summary.as_deref(), Some("A plain-JSON thread"));
    }
}
