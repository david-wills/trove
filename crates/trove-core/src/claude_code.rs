//! Claude Code — periodic local sync of the CLI's own session transcripts.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/claude-code.md.
//!
//! A **Periodic** local-file collector (the [`crate::imessage`] shape, but no
//! TCC permission — these are plain home-directory files). Claude Code writes
//! one JSONL file per session under `<base>/projects/<slug>/<session-id>.jsonl`
//! (`<base>` = `CLAUDE_CONFIG_DIR` or `~/.claude`), one typed record per line.
//! The format is *community-documented*, not official (Yi Huang's session-file
//! article; simonw/claude-code-transcripts; raine/claude-history), so every
//! parse is tolerant: unknown record types and malformed lines are counted and
//! skipped, never fatal.
//!
//! **`developer/` is raw-only** (taxonomy decision): no media-plays/domain
//! contract, no normalized struct — this module owns its row shape.
//!
//! ## Two layers, one privacy line
//!
//! - **Metadata stream (default, on whenever the integration is enabled)** —
//!   `developer/claude-code/YYYY-MM.jsonl`, ONE [`SessionRow`] per session,
//!   partitioned by the session's *start* month. It carries counts, per-tool
//!   call tallies, start/end timestamps, the derived project name/path, and the
//!   auto-generated session title — and **no conversation content**: no prompt
//!   text, no assistant text, no tool inputs/outputs, no code. The auto-title
//!   (Claude Code's short generated session name) is the one human-readable
//!   string allowed by default.
//! - **Transcript sidecars (opt-in only)** — ONLY when the
//!   `claude-code-transcripts` toggle is on, the session's full message records
//!   are *additionally* copied verbatim to
//!   `developer/claude-code/transcripts/<session-id>.jsonl` (full fidelity, the
//!   conversation content). With the opt-in off these files are never written.
//!
//! ## Scan / cursor / upsert
//!
//! A live session's file grows in place as the conversation continues, so this
//! is **not** a pure-append source. The cursor (`.trove/claude-code-sync.json`,
//! rebuildable) maps `session_id → last-seen file mtime`; each pass reprocesses
//! only sessions whose file mtime advanced. A session's start month is stable,
//! so its row stays in the same `YYYY-MM.jsonl` partition — we **upsert by
//! `session_id`** within that partition (read, replace the matching line,
//! rewrite; append when new) so a re-scanned session updates in place instead
//! of duplicating. `guid == session_id`.

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

/// Hourly, like the other always-on local collectors. Sessions append over
/// minutes/hours; an hourly sweep keeps the metadata fresh without churn.
pub const CLAUDE_CODE_SYNC_SECS: u64 = 3600;

/// Contract-free metadata stream (one row per session).
const DIR: &str = "developer/claude-code";
/// Opt-in full-transcript sidecars, one file per session.
const TRANSCRIPTS_DIR: &str = "developer/claude-code/transcripts";
/// Rebuildable cursor: session_id → last-seen file mtime (unix ms).
const SYNC_FILE: &str = ".trove/claude-code-sync.json";
/// Opt-in sub-arm id, consulted inside the scan.
const TRANSCRIPTS_ID: &str = "claude-code-transcripts";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_claude_code()?;
    Ok(crate::registry::CollectOutcome::note_if(s.sessions > 0, || {
        let mut note = format!("claude code synced — {} sessions", s.sessions);
        if s.transcripts > 0 {
            note.push_str(&format!(", {} transcripts", s.transcripts));
        }
        if s.skipped_records > 0 {
            note.push_str(&format!(" ({} records skipped)", s.skipped_records));
        }
        note
    }))
}

// Manual "Sync now": the same scan, surfacing a human headline.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_claude_code()?;
    let headline = if s.sessions == 0 {
        "Claude Code is up to date — no changed sessions".to_string()
    } else {
        format!("Claude Code synced — {} sessions updated", s.sessions)
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
/// arm; the full-transcript sidecar is the [`TRANSCRIPTS_DEF`] opt-in,
/// consulted inside the scan.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "claude-code",
        name: "Claude Code",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Captures your Claude Code session history — timestamps, \
                      project context, per-tool call counts, and the \
                      auto-generated session title — from the local JSONL \
                      transcript files written by the CLI.",
        domain: "developer",
        vault_path: "developer/claude-code/",
        toggleable: true,
        setup: &[
            "Reads the session files Claude Code already writes under ~/.claude/projects — nothing to install or connect.",
            "Set CLAUDE_CONFIG_DIR if your Claude Code config lives somewhere other than ~/.claude.",
        ],
        caveats: "Only session metadata and the auto-generated title are stored by default — never prompts, \
                  responses, tool inputs/outputs, or code. Turn on \u{201C}Claude Code — full \
                  transcripts\u{201D} to also save the conversation content. The CLAUDE_CONFIG_DIR \
                  environment variable overrides the default ~/.claude path.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(CLAUDE_CODE_SYNC_SECS),
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

/// Registered in [`crate::integrations::INTEGRATIONS`]. The full-transcript
/// opt-in: covered by [`DEF`]'s pass (no pass of its own), default-off. When
/// on, the scan additionally writes each session's conversation content to
/// `developer/claude-code/transcripts/<session-id>.jsonl`. Like every
/// `CoveredBy` sub-arm (chrome-history, screen-time-this-mac) it has no INDEX
/// row and no brief.
pub static TRANSCRIPTS_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: TRANSCRIPTS_ID,
        name: "Claude Code — full transcripts",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Additionally stores the full text of every Claude Code session — your prompts, \
                      Claude's responses, and tool calls — as a sidecar transcript per session. \
                      Off by default; the Claude Code card stores only metadata without it.",
        domain: "developer",
        vault_path: "developer/claude-code/transcripts/",
        toggleable: true,
        setup: &["Enable only if you want the complete conversation text saved, not just session metadata."],
        caveats: "Stores the complete conversation content — every prompt, response, and tool input/output, \
                  including any code and file contents that appeared in the session. Opt-in for exactly that \
                  reason; leave it off to keep only metadata. Runs with the Claude Code collector's pass.",
    },
    behavior: Behavior::CoveredBy("claude-code"),
    permission: None,
    last_data: Some(transcripts_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// The metadata row (raw, this module's own shape — no contract).

/// One row in `developer/claude-code/YYYY-MM.jsonl`: a session's metadata, with
/// **no conversation content**. The auto-generated title is the only
/// human-readable string here, by design.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SessionRow {
    /// Stable id, equal to the session's filename stem. `guid == session_id`.
    pub session_id: String,
    /// Human project name (the last path component of `project_path`).
    pub project: String,
    /// Absolute working directory of the session (from records' `cwd`, else
    /// decoded from the project-dir slug).
    pub project_path: String,
    /// RFC3339 local time of the first timestamped record.
    pub start_ts: String,
    /// RFC3339 local time of the last timestamped record.
    pub end_ts: String,
    /// User + assistant turns in the session.
    pub message_count: u64,
    /// Genuine user prompts (turns that are purely tool results don't count).
    pub user_message_count: u64,
    /// Assistant turns.
    pub assistant_message_count: u64,
    /// Calls per tool name, e.g. `{"Bash":5,"Edit":3}`. Empty when none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_calls: BTreeMap<String, u64>,
    /// The auto-generated session title, when Claude Code produced one. Omitted
    /// when absent. This is metadata (a short label), not conversation content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Result of one scan pass, for logging/status.
#[derive(Debug, Clone, Default)]
pub struct ClaudeCodeStats {
    /// Sessions written or updated this pass.
    pub sessions: u64,
    /// Transcript sidecars written this pass (0 unless the opt-in is on).
    pub transcripts: u64,
    /// Malformed lines + unknown-typed records skipped (a few are normal;
    /// a surge means format drift).
    pub skipped_records: u64,
}

/// Everything one session file decodes to: the metadata row, the verbatim
/// content records (for the opt-in sidecar), and a skip tally.
struct ParsedSession {
    row: SessionRow,
    /// The raw message records, verbatim — written only when the opt-in is on.
    transcript: Vec<Value>,
    skipped: u64,
}

// ---------------------------------------------------------------------------
// Paths.

/// Base config dir: `CLAUDE_CONFIG_DIR` when set and non-empty, else `~/.claude`.
fn claude_base_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Every per-session file: `<base>/projects/<slug>/<session-id>.jsonl`. Each
/// entry is (session_id, project-slug, path). Empty when the tree is missing.
fn session_files(base: &Path) -> Vec<(String, String, PathBuf)> {
    let projects = base.join("projects");
    let Ok(slugs) = fs::read_dir(&projects) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for slug_entry in slugs.flatten() {
        let slug_path = slug_entry.path();
        if !slug_path.is_dir() {
            continue;
        }
        let slug = slug_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(&slug_path) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) {
                out.push((stem, slug.clone(), p));
            }
        }
    }
    // Deterministic passes.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Best-effort decode of a project-dir slug back to a path. Claude Code forms
/// the slug by replacing `/` with `-` in the cwd, which is lossy (real dashes
/// also become `-`), so this is only a *fallback* for `project_path` — a
/// record's `cwd` is authoritative and preferred. We map dashes back to `/`
/// and re-root at `/`.
fn decode_slug(slug: &str) -> String {
    if slug.is_empty() {
        return String::new();
    }
    slug.replace('-', "/")
}

/// File mtime in unix milliseconds, for the cursor.
fn file_mtime_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

/// The last path component (the human project name).
fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

// ---------------------------------------------------------------------------
// Tolerant parse (pure, fixture-tested).

/// A user record whose entire `message.content` is `tool_result` blocks is a
/// tool-result turn the CLI logs as `type:"user"`, not a real prompt — it
/// doesn't count toward `user_message_count`. A string-content user message,
/// or one carrying any non-tool_result block, is a genuine prompt.
fn user_is_real_prompt(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("type").and_then(Value::as_str) != Some("tool_result")
        }),
        // No/!object content: treat as a prompt (tolerant — better to count a
        // turn than to silently drop it).
        _ => true,
    }
}

/// Tally `tool_use` blocks (by `name`) inside one assistant `message.content`.
fn count_tool_uses(message: &Value, tally: &mut BTreeMap<String, u64>) {
    if let Some(Value::Array(blocks)) = message.get("content") {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = b
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown");
                *tally.entry(name.to_string()).or_insert(0) += 1;
            }
        }
    }
}

/// Parse one session file's body into a [`ParsedSession`]. Tolerant by
/// contract: a malformed line or an unknown record type is counted in
/// `skipped` and skipped — never an error. `session_id` is the filename stem
/// (authoritative); `slug` seeds the `project_path` fallback. Returns `None`
/// only when the file yields no usable record at all.
fn parse_session(body: &str, session_id: &str, slug: &str) -> Option<ParsedSession> {
    let mut message_count = 0u64;
    let mut user_message_count = 0u64;
    let mut assistant_message_count = 0u64;
    let mut tool_calls: BTreeMap<String, u64> = BTreeMap::new();
    let mut summary: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut min_ts: Option<String> = None;
    let mut max_ts: Option<String> = None;
    let mut transcript: Vec<Value> = Vec::new();
    let mut skipped = 0u64;
    let mut saw_record = false;

    let mut note_ts = |ts: &str| {
        // RFC3339 sorts lexically, so plain string min/max is correct for the
        // canonical `...Z` / `...±hh:mm` forms Claude Code emits.
        if min_ts.as_deref().is_none_or(|m| ts < m) {
            min_ts = Some(ts.to_string());
        }
        if max_ts.as_deref().is_none_or(|m| ts > m) {
            max_ts = Some(ts.to_string());
        }
    };

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            skipped += 1; // malformed line — skip, never fatal
            continue;
        };
        saw_record = true;
        let rtype = rec.get("type").and_then(Value::as_str).unwrap_or("");

        // Capture the real cwd from any record that carries one (authoritative
        // project path).
        if cwd.is_none() {
            if let Some(c) = rec.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                cwd = Some(c.to_string());
            }
        }
        // Any record's timestamp widens the [start, end] span.
        if let Some(ts) = rec.get("timestamp").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            note_ts(ts);
        }

        match rtype {
            "user" => {
                let message = rec.get("message").cloned().unwrap_or(Value::Null);
                message_count += 1;
                if user_is_real_prompt(&message) {
                    user_message_count += 1;
                }
                transcript.push(rec);
            }
            "assistant" => {
                let message = rec.get("message").cloned().unwrap_or(Value::Null);
                message_count += 1;
                assistant_message_count += 1;
                count_tool_uses(&message, &mut tool_calls);
                transcript.push(rec);
            }
            // The auto-generated session title. Claude Code emits it as an
            // `ai-title` record (field `aiTitle`); the community docs also
            // describe a top-level `summary` record (field `summary`) — accept
            // either, last one wins.
            "ai-title" => {
                if let Some(t) = rec.get("aiTitle").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    summary = Some(t.to_string());
                }
            }
            "summary" => {
                if let Some(t) = rec.get("summary").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    summary = Some(t.to_string());
                }
            }
            // Known-but-uncounted bookkeeping records (mode, permission-mode,
            // file-history-snapshot, last-prompt, system, attachment, …) and
            // any unknown future type: not conversation turns, so they neither
            // count nor land in the transcript. An empty/unknown type that
            // carried no timestamp and no cwd is genuine noise → tally it.
            other => {
                if other.is_empty() {
                    skipped += 1;
                }
            }
        }
    }

    if !saw_record {
        return None;
    }

    let project_path = cwd.unwrap_or_else(|| decode_slug(slug));
    let project = basename(&project_path);
    // A session with no timestamped record is degenerate — only bookkeeping
    // records, no real turn yet. Skip it (like the no-record case above) rather
    // than invent a `now()` start: a fabricated month would misfile the row and,
    // on a later re-scan in a different month, orphan a stale copy in the old
    // partition (upsert only dedupes within one partition key). It is picked up
    // for real once it has a timestamped turn (every real session does).
    let Some(start_ts) = min_ts else {
        return None;
    };
    let end_ts = max_ts.unwrap_or_else(|| start_ts.clone());

    Some(ParsedSession {
        row: SessionRow {
            session_id: session_id.to_string(),
            project,
            project_path,
            start_ts,
            end_ts,
            message_count,
            user_message_count,
            assistant_message_count,
            tool_calls,
            summary,
        },
        transcript,
        skipped,
    })
}

// ---------------------------------------------------------------------------
// Cursor.

/// Incremental-sync state, persisted in `.trove/claude-code-sync.json`.
/// `mtimes` (session_id → last-seen file mtime, unix ms) is the real state;
/// it's rebuildable by re-scanning (a lost cursor just reprocesses every
/// session, and the upsert keeps that idempotent).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeCodeSyncState {
    /// RFC3339 local time of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// session_id → newest file mtime processed (unix ms).
    #[serde(default)]
    pub mtimes: BTreeMap<String, i64>,
}

impl Vault {
    fn read_claude_code_sync(&self) -> ClaudeCodeSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_claude_code_sync(&self, state: &ClaudeCodeSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    // -----------------------------------------------------------------------
    // The scan.

    /// One incremental scan pass over the Claude Code session files. Silently a
    /// no-op when the base dir is missing (Claude Code never run / wrong path).
    /// The opt-in `claude-code-transcripts` toggle is consulted here.
    pub fn collect_claude_code(&self) -> Result<ClaudeCodeStats> {
        let Some(base) = claude_base_dir() else {
            return Ok(ClaudeCodeStats::default());
        };
        let want_transcripts = self.integration_enabled(TRANSCRIPTS_ID);
        self.collect_claude_code_from(&base, want_transcripts)
    }

    /// The pass itself, base-dir- and opt-in-injected for tests.
    pub(crate) fn collect_claude_code_from(
        &self,
        base: &Path,
        want_transcripts: bool,
    ) -> Result<ClaudeCodeStats> {
        let mut stats = ClaudeCodeStats::default();
        let files = session_files(base);
        if files.is_empty() {
            return Ok(stats);
        }
        let mut state = self.read_claude_code_sync();
        let mut changed = false;

        for (session_id, slug, path) in files {
            let Some(mtime) = file_mtime_ms(&path) else {
                continue;
            };
            // mtime-gated: only (re)process a session whose file advanced.
            if state.mtimes.get(&session_id) == Some(&mtime) {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue; // unreadable now — retry next pass (cursor untouched)
            };
            let Some(parsed) = parse_session(&body, &session_id, &slug) else {
                // No usable record; remember the mtime so we don't reread it.
                state.mtimes.insert(session_id, mtime);
                changed = true;
                continue;
            };
            stats.skipped_records += parsed.skipped;

            // Upsert the metadata row into its start-month partition.
            if let Err(e) = self.upsert_session_row(&parsed.row) {
                eprintln!("trove claude-code: upsert for {session_id} failed: {e:#}");
                continue; // cursor untouched → retried next pass
            }

            // Opt-in only: write the full conversation sidecar. When the
            // toggle is off this branch never runs, so no transcript file is
            // ever created.
            if want_transcripts {
                if let Err(e) = self.write_transcript(&session_id, &parsed.transcript) {
                    eprintln!("trove claude-code: transcript for {session_id} failed: {e:#}");
                    continue; // cursor untouched → retried next pass
                }
                stats.transcripts += 1;
            }

            state.mtimes.insert(session_id, mtime);
            stats.sessions += 1;
            changed = true;
        }

        if changed {
            state.updated = Local::now().to_rfc3339();
            self.write_claude_code_sync(&state)?;
        }
        Ok(stats)
    }

    /// Upsert one [`SessionRow`] into `developer/claude-code/YYYY-MM.jsonl`
    /// (keyed by the session's start month). Because the start month is stable,
    /// a re-scanned session always lands in the same partition: we replace the
    /// line whose `session_id` matches (its counts/end_ts grew) and append when
    /// it's new — so a growing live session never duplicates. The partition is
    /// rewritten atomically.
    fn upsert_session_row(&self, row: &SessionRow) -> Result<()> {
        let key = Partition::Month.key(&row.start_ts).with_context(|| {
            format!(
                "claude-code session {} has an unpartitionable start_ts {:?}",
                row.session_id, row.start_ts
            )
        })?;
        let stream = self.stream(DIR, Partition::Month);
        let mut rows: Vec<SessionRow> = stream.read(key)?;
        match rows.iter_mut().find(|r| r.session_id == row.session_id) {
            Some(existing) => *existing = row.clone(),
            None => rows.push(row.clone()),
        }
        // Re-write the whole partition (the row count per month is small).
        self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &rows)
    }

    /// Write one session's full transcript sidecar verbatim, atomically. Only
    /// called when the `claude-code-transcripts` opt-in is on.
    fn write_transcript(&self, session_id: &str, records: &[Value]) -> Result<()> {
        self.write_snapshot(&format!("{TRANSCRIPTS_DIR}/{session_id}.jsonl"), records)
    }

    // -----------------------------------------------------------------------
    // Reads.

    /// All session rows for one month (`YYYY-MM`), in file order. The hub's
    /// Recent-data view reads this.
    pub fn claude_code_sessions(&self, month: &str) -> Result<Vec<SessionRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-claudecode-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A unique temp base dir holding the `projects/<slug>/` tree.
    fn temp_base(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-ccbase-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Write a synthetic session file under `<base>/projects/<slug>/<id>.jsonl`,
    /// returning its path. All content here is HAND-BUILT synthetic data.
    fn write_session(base: &Path, slug: &str, id: &str, body: &str) -> PathBuf {
        let dir = base.join("projects").join(slug);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, body).unwrap();
        path
    }

    /// Bump a file's mtime forward so the cursor sees it as changed (the test
    /// stand-in for a live session appending). Dependency-free via the stable
    /// `File::set_modified`.
    fn bump_mtime(path: &Path, secs_ahead: u64) {
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(secs_ahead);
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    /// A full synthetic session: user prompt, assistant turn with two tool_use
    /// blocks (Bash, Edit), a user tool_result turn (NOT a real prompt), a
    /// second assistant turn with one Bash tool_use, an ai-title, and assorted
    /// bookkeeping records that must be ignored. The known prompt string
    /// "SYNTHETIC_SECRET_PROMPT" lets the privacy test assert it never leaks.
    fn rich_session_body() -> &'static str {
        concat!(
            r#"{"type":"mode","sessionId":"sess-rich","mode":"default"}"#, "\n",
            r#"{"type":"user","sessionId":"sess-rich","cwd":"/Users/dev/code/my-project","timestamp":"2026-06-10T09:00:00Z","message":{"role":"user","content":"SYNTHETIC_SECRET_PROMPT please refactor"}}"#, "\n",
            r#"{"type":"assistant","sessionId":"sess-rich","cwd":"/Users/dev/code/my-project","timestamp":"2026-06-10T09:00:05Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"PRIVATE_THOUGHT"},{"type":"text","text":"PRIVATE_ASSISTANT_TEXT"},{"type":"tool_use","name":"Bash","input":{"command":"PRIVATE_CMD"}},{"type":"tool_use","name":"Edit","input":{"path":"x"}}]}}"#, "\n",
            r#"{"type":"user","sessionId":"sess-rich","cwd":"/Users/dev/code/my-project","timestamp":"2026-06-10T09:00:10Z","message":{"role":"user","content":[{"type":"tool_result","content":"PRIVATE_TOOL_OUTPUT"}]}}"#, "\n",
            r#"{"type":"assistant","sessionId":"sess-rich","cwd":"/Users/dev/code/my-project","timestamp":"2026-06-10T09:05:00Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"PRIVATE_CMD2"}}]}}"#, "\n",
            r#"{"type":"ai-title","sessionId":"sess-rich","aiTitle":"Refactor the widget module"}"#, "\n",
            r#"{"type":"file-history-snapshot","messageId":"m1","snapshot":{},"isSnapshotUpdate":false}"#, "\n"
        )
    }

    /// A session with NO ai-title/summary record — `summary` must be omitted.
    fn summaryless_body() -> &'static str {
        concat!(
            r#"{"type":"user","sessionId":"sess-bare","cwd":"/tmp/bare","timestamp":"2026-05-02T12:00:00Z","message":{"role":"user","content":"hello"}}"#, "\n",
            r#"{"type":"assistant","sessionId":"sess-bare","cwd":"/tmp/bare","timestamp":"2026-05-02T12:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#, "\n"
        )
    }

    #[test]
    fn parses_metadata_counts_tools_and_summary() {
        let p = parse_session(rich_session_body(), "sess-rich", "-Users-dev-code-my-project").unwrap();
        let r = &p.row;
        assert_eq!(r.session_id, "sess-rich", "guid == session_id == file stem");
        // cwd wins over the lossy slug decode.
        assert_eq!(r.project_path, "/Users/dev/code/my-project");
        assert_eq!(r.project, "my-project");
        // 2 user + 2 assistant = 4 turns; only one user turn is a real prompt
        // (the other is a tool_result turn).
        assert_eq!(r.message_count, 4);
        assert_eq!(r.user_message_count, 1, "tool_result user turn is not a prompt");
        assert_eq!(r.assistant_message_count, 2);
        // tool_use blocks tallied by name across assistant turns.
        assert_eq!(r.tool_calls.get("Bash"), Some(&2));
        assert_eq!(r.tool_calls.get("Edit"), Some(&1));
        assert_eq!(r.tool_calls.len(), 2);
        // start/end span the first/last timestamped records.
        assert_eq!(r.start_ts, "2026-06-10T09:00:00Z");
        assert_eq!(r.end_ts, "2026-06-10T09:05:00Z");
        // ai-title becomes the summary.
        assert_eq!(r.summary.as_deref(), Some("Refactor the widget module"));
        // The transcript captured exactly the 4 message records (not the
        // mode/snapshot bookkeeping).
        assert_eq!(p.transcript.len(), 4);
        assert_eq!(p.skipped, 0);
    }

    #[test]
    fn summaryless_session_omits_summary() {
        let p = parse_session(summaryless_body(), "sess-bare", "-tmp-bare").unwrap();
        assert_eq!(p.row.summary, None, "no title record → summary omitted");
        // Confirm it serializes without a `summary` key.
        let json = serde_json::to_string(&p.row).unwrap();
        assert!(!json.contains("\"summary\""), "absent summary not serialized: {json}");
        assert_eq!(p.row.message_count, 2);
        assert_eq!(p.row.user_message_count, 1);
    }

    #[test]
    fn slug_decode_is_fallback_only() {
        // No cwd anywhere → fall back to the decoded slug.
        let body = concat!(
            r#"{"type":"user","sessionId":"s","timestamp":"2026-06-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#, "\n"
        );
        let p = parse_session(body, "s", "-Users-dev-proj").unwrap();
        assert_eq!(p.row.project_path, "/Users/dev/proj");
        assert_eq!(p.row.project, "proj");
    }

    #[test]
    fn tolerant_parse_skips_unknown_and_malformed() {
        let body = concat!(
            r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"2026-06-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#, "\n",
            "this is not json at all\n",                          // malformed → skip+count
            r#"{"type":"totally-unknown-future-type","foo":1}"#, "\n", // unknown but typed → skip silently
            r#"{"no_type_field":true}"#, "\n",                    // empty type, noise → counted
            "\n",                                                  // blank → ignored
            r#"{"type":"assistant","sessionId":"s","cwd":"/p","timestamp":"2026-06-01T00:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#, "\n"
        );
        let p = parse_session(body, "s", "-p").unwrap();
        assert_eq!(p.row.message_count, 2, "only the two real turns counted");
        // malformed line + the typeless noise record = 2 skipped; the
        // known-but-uncounted unknown type is skipped silently (not tallied).
        assert_eq!(p.skipped, 2);
    }

    #[test]
    fn scan_writes_metadata_only_by_default_no_transcripts() {
        let v = temp_vault("default");
        let base = temp_base("default");
        write_session(&base, "-Users-dev-code-my-project", "sess-rich", rich_session_body());

        // Default run: opt-in OFF.
        let stats = v.collect_claude_code_from(&base, false).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 0, "no transcripts when opt-in off");

        // The metadata row landed in the start-month partition.
        let rows = v.claude_code_sessions("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "sess-rich");
        assert_eq!(rows[0].tool_calls.get("Bash"), Some(&2));

        // CRITICAL: no transcript directory, no transcript files.
        assert!(
            !v.root().join("developer/claude-code/transcripts").exists(),
            "transcripts/ must not exist when the opt-in is off"
        );

        // CRITICAL privacy assertion: the raw metadata partition file contains
        // NO conversation content — not the prompt, assistant text, thoughts,
        // tool inputs, or tool outputs.
        let partition =
            fs::read_to_string(v.root().join("developer/claude-code/2026-06.jsonl")).unwrap();
        for leak in [
            "SYNTHETIC_SECRET_PROMPT",
            "PRIVATE_THOUGHT",
            "PRIVATE_ASSISTANT_TEXT",
            "PRIVATE_CMD",
            "PRIVATE_CMD2",
            "PRIVATE_TOOL_OUTPUT",
        ] {
            assert!(
                !partition.contains(leak),
                "metadata row leaked conversation content {leak:?}: {partition}"
            );
        }
        // The auto-title (metadata, allowed) IS present.
        assert!(partition.contains("Refactor the widget module"));
    }

    #[test]
    fn opt_in_writes_full_transcript_sidecar() {
        let v = temp_vault("optin");
        let base = temp_base("optin");
        write_session(&base, "-Users-dev-code-my-project", "sess-rich", rich_session_body());

        // Opt-in ON.
        let stats = v.collect_claude_code_from(&base, true).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.transcripts, 1);

        // Sidecar exists, named by session id, and DOES carry the full content.
        let sidecar = v
            .root()
            .join("developer/claude-code/transcripts/sess-rich.jsonl");
        assert!(sidecar.exists(), "opt-in writes the sidecar");
        let body = fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("SYNTHETIC_SECRET_PROMPT"), "full fidelity in the sidecar");
        assert!(body.contains("PRIVATE_TOOL_OUTPUT"));
        // Only the 4 message records (bookkeeping excluded from the transcript).
        assert_eq!(body.lines().filter(|l| !l.trim().is_empty()).count(), 4);

        // The metadata row still carries no content even with the opt-in on.
        let partition =
            fs::read_to_string(v.root().join("developer/claude-code/2026-06.jsonl")).unwrap();
        assert!(!partition.contains("SYNTHETIC_SECRET_PROMPT"));
    }

    #[test]
    fn rescan_on_append_upserts_in_place() {
        let v = temp_vault("upsert");
        let base = temp_base("upsert");
        // Two sessions in the same start month.
        let p_rich = write_session(&base, "-p", "sess-rich", rich_session_body());
        // sess-other is independent and in the same June partition.
        let other = concat!(
            r#"{"type":"user","sessionId":"sess-other","cwd":"/o","timestamp":"2026-06-11T08:00:00Z","message":{"role":"user","content":"q"}}"#, "\n",
            r#"{"type":"assistant","sessionId":"sess-other","cwd":"/o","timestamp":"2026-06-11T08:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"a"}]}}"#, "\n"
        );
        write_session(&base, "-o", "sess-other", other);

        let first = v.collect_claude_code_from(&base, false).unwrap();
        assert_eq!(first.sessions, 2);
        let rows = v.claude_code_sessions("2026-06").unwrap();
        assert_eq!(rows.len(), 2, "two distinct sessions, two rows");
        let rich_before = rows.iter().find(|r| r.session_id == "sess-rich").unwrap();
        assert_eq!(rich_before.message_count, 4);

        // Re-scan with no file change → nothing reprocessed.
        let noop = v.collect_claude_code_from(&base, false).unwrap();
        assert_eq!(noop.sessions, 0, "mtime gate: unchanged files skipped");

        // Grow sess-rich (a live session appended two more turns) and advance
        // its mtime; sess-other untouched.
        let grown = format!(
            "{}{}{}",
            rich_session_body(),
            r#"{"type":"user","sessionId":"sess-rich","cwd":"/Users/dev/code/my-project","timestamp":"2026-06-10T09:10:00Z","message":{"role":"user","content":"more"}}"#,
            concat!("\n", r#"{"type":"assistant","sessionId":"sess-rich","cwd":"/Users/dev/code/my-project","timestamp":"2026-06-10T09:11:00Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#, "\n")
        );
        fs::write(&p_rich, grown).unwrap();
        bump_mtime(&p_rich, 60);

        let second = v.collect_claude_code_from(&base, false).unwrap();
        assert_eq!(second.sessions, 1, "only the changed session reprocessed");

        // UPSERT: still exactly two rows; sess-rich updated in place.
        let rows = v.claude_code_sessions("2026-06").unwrap();
        assert_eq!(rows.len(), 2, "upsert replaced, did not duplicate");
        let rich_after = rows.iter().find(|r| r.session_id == "sess-rich").unwrap();
        assert_eq!(rich_after.message_count, 6, "grew by the two new turns");
        assert_eq!(rich_after.end_ts, "2026-06-10T09:11:00Z", "end_ts advanced");
        assert_eq!(rich_after.tool_calls.get("Bash"), Some(&3), "new Bash call tallied");
        // sess-other is byte-for-byte unchanged.
        let other_after = rows.iter().find(|r| r.session_id == "sess-other").unwrap();
        assert_eq!(other_after.message_count, 2);
        let _ = other;
    }

    #[test]
    fn config_dir_override_is_followed() {
        // Point CLAUDE_CONFIG_DIR at a temp dir and confirm claude_base_dir
        // resolves to it. Serialized via a process-wide guard isn't needed: we
        // restore immediately and assert on the returned path.
        let base = temp_base("override");
        fs::create_dir_all(base.join("projects")).unwrap();
        let prev = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", &base);
        let resolved = claude_base_dir().unwrap();
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        assert_eq!(resolved, base, "CLAUDE_CONFIG_DIR overrides the base dir");

        // And a scan against an explicit base follows that tree.
        let v = temp_vault("override");
        write_session(&base, "-x", "s1", summaryless_body());
        let stats = v.collect_claude_code_from(&base, false).unwrap();
        assert_eq!(stats.sessions, 1, "scan reads from the injected base dir");
        assert_eq!(v.claude_code_sessions("2026-05").unwrap().len(), 1);
    }

    #[test]
    fn cursor_back_compat_old_and_empty_deserialize() {
        // Empty object (a brand-new/missing cursor).
        let empty: ClaudeCodeSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.mtimes.is_empty());
        assert_eq!(empty.updated, "");
        // An "old" cursor that only ever wrote `updated` (no mtimes key) still
        // deserializes, defaulting mtimes to empty.
        let old: ClaudeCodeSyncState =
            serde_json::from_str(r#"{"updated":"2026-01-01T00:00:00-08:00"}"#).unwrap();
        assert_eq!(old.updated, "2026-01-01T00:00:00-08:00");
        assert!(old.mtimes.is_empty());
        // Round-trips.
        let mut s = ClaudeCodeSyncState::default();
        s.mtimes.insert("abc".into(), 123);
        let json = serde_json::to_string(&s).unwrap();
        let back: ClaudeCodeSyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mtimes.get("abc"), Some(&123));
    }

    #[test]
    fn missing_base_dir_is_a_quiet_noop() {
        let v = temp_vault("missing");
        let base = temp_base("missing"); // never created
        let stats = v.collect_claude_code_from(&base, false).unwrap();
        assert_eq!(stats.sessions, 0);
        assert!(!v.root().join("developer/claude-code").exists());
    }
}
