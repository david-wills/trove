//! Org-mode / plain-text task files — watch-folder parser for `.org` TODO files.
//!
//! Reads every `.org` file in a user-configured folder (and subdirectories),
//! extracts TODO/DONE tasks, and feeds the already-bound [`crate::tasks`]
//! contract. Works with Emacs Org-mode, beorg (iCloud), Logseq-org, and
//! Doom/Spacemacs without any account or login.
//!
//! ## Format
//!
//! Org headings have the form:
//! ```text
//! * [TODO|DONE|WAITING|…] [#A] Heading title :tag1:tag2:
//!   SCHEDULED: <2024-01-15 Mon>  DEADLINE: <2024-01-20 Sat>
//!   CLOSED: [2024-01-19 Fri 14:30]
//!   :PROPERTIES:
//!   :ID: some-uuid-here
//!   :END:
//!   :LOGBOOK:
//!   CLOCK: [2024-01-18 Thu 10:00]--[2024-01-18 Thu 11:30] =>  1:30
//!   - State "DONE" from "TODO" [2024-01-19 Fri 14:30]
//!   :END:
//! ```
//!
//! ## GUID strategy
//!
//! - If the heading carries an `:ID:` property, use it — stable across file renames.
//! - Otherwise hash `"<file_rel>|<heading_text>|<outline_position>"` with SHA-256
//!   (truncated to 16 hex chars) — idempotent across re-parses of the same file
//!   but changes if the heading is renamed or moved.
//!
//! ## Vault layout
//!
//! - Raw layer: `tasks/org-mode/raw/<file_slug>-YYYY-MM.jsonl` (one record per
//!   task, upserted by guid, partitioned by modified-or-created month; the raw
//!   record carries the full parsed fields for full fidelity).
//! - Contract layer: `tasks/org-mode/` via [`crate::tasks::apply_tasks_sync`].
//! - Config: `.trove/org-mode-config.json` holds `{ "folder": "/path/to/org" }`.
//!   Without it the collector is a silent no-op.
//!
//! ## Fate resolution
//!
//! Because org files are local plain text, the fate of a task that disappeared
//! from the snapshot is resolved by re-scanning the files:
//! - If the task's guid appears with `DONE` (or any closed keyword) in any file
//!   → `TaskFate::Completed` with the `CLOSED:` timestamp.
//! - If the file that contained the task no longer exists → `TaskFate::Unknown`
//!   (carry forward; the file may be temporarily moved or on an unmounted volume).
//! - Otherwise (file still present, task heading gone) → `TaskFate::Deleted`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::tasks::{ProjectInfo, Task, TaskFate, TASKS_SYNC_SECS};
use crate::vault::Vault;

/// Source id — folder name under `tasks/`, key in `.trove/tasks-sync.json`.
const SOURCE: &str = "org-mode";
/// Config file: `{ "folder": "/absolute/path" }`.
const CONFIG_FILE: &str = ".trove/org-mode-config.json";
/// Raw firehose directory.
const RAW_DIR: &str = "tasks/org-mode/raw";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match collect_org(vault) {
        Ok(stats) => Ok(CollectOutcome::note_if(
            stats.created + stats.completed + stats.deleted > 0,
            || {
                format!(
                    "org-mode synced — {} open, {} completed, {} deleted",
                    stats.open, stats.completed, stats.deleted
                )
            },
        )),
        Err(e) => Ok(CollectOutcome::note(format!("org-mode sync skipped: {e}"))),
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, SOURCE)
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let stats = collect_org(vault)?;
    let headline = format!(
        "org-mode synced — {} open tasks, {} completed, {} deleted",
        stats.open, stats.completed, stats.deleted
    );
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("open", stats.open),
            ("created", stats.created),
            ("completed", stats.completed),
            ("deleted", stats.deleted),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "org-mode",
        name: "Org-mode / Plain-text Tasks",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Watches a folder of .org files every 15 minutes and extracts TODO items, \
                      priorities, tags, and scheduling into the unified task store. Works with \
                      Emacs Org-mode, beorg (iCloud), Logseq-org, and Doom/Spacemacs — no \
                      account or login required.",
        domain: "tasks",
        vault_path: "tasks/org-mode/",
        toggleable: true,
        setup: &[
            "Point the collector at your org folder in Trove's settings \
             (e.g. ~/org, ~/Documents/notes, or the beorg iCloud path).",
            "Any .org files already in that folder are parsed on the next sync.",
        ],
        caveats: "Task identity relies on the :ID: property when set, \
                  or a hash of the heading text and file path otherwise — \
                  renaming a heading without an :ID: property is treated as a \
                  new task.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(TASKS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Config.

/// User configuration stored in [`CONFIG_FILE`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OrgConfig {
    /// Absolute path to the folder containing `.org` files.
    #[serde(default)]
    pub folder: String,
}

impl Vault {
    fn read_org_config(&self) -> OrgConfig {
        self.resolve(CONFIG_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Org parser — pure functions, no I/O.

/// A known TODO keyword (state).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TodoState {
    /// A todo/in-progress keyword (open task).
    Open(String),
    /// A done/closed keyword (completed task).
    Done(String),
}

impl TodoState {
    fn keyword(&self) -> &str {
        match self {
            TodoState::Open(k) | TodoState::Done(k) => k,
        }
    }
    fn is_done(&self) -> bool {
        matches!(self, TodoState::Done(_))
    }
}

/// Classify an org keyword as Open or Done. The set of "done" keywords is
/// configurable per-org-file via `#+TODO:` directives; we cover the most
/// common conventions.
fn classify_keyword(kw: &str) -> Option<TodoState> {
    match kw {
        // Standard done keywords.
        "DONE" | "CANCELLED" | "CANCELED" | "CLOSED" | "FIXED" | "ARCHIVED" => {
            Some(TodoState::Done(kw.to_string()))
        }
        // Standard open/in-progress keywords.
        "TODO" | "NEXT" | "DOING" | "IN-PROGRESS" | "WAITING" | "HOLD" | "SOMEDAY"
        | "STARTED" | "ACTIVE" => Some(TodoState::Open(kw.to_string())),
        // Unknown → not a task heading.
        _ => None,
    }
}

/// A fully-parsed org task heading (all optional fields included).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OrgTask {
    /// The stable dedup guid (`:ID:` property or position hash).
    pub guid: String,
    /// Level (number of asterisks), 1-indexed.
    pub level: usize,
    /// TODO/DONE/WAITING/… keyword.
    pub keyword: String,
    /// Whether the keyword is a "done" state.
    pub done: bool,
    /// Priority cookie: "A", "B", "C", or empty.
    pub priority: String,
    /// Heading title (keyword, priority, and tags stripped).
    pub title: String,
    /// Org tags (e.g., `:work:urgent:` → ["work", "urgent"]).
    pub tags: Vec<String>,
    /// SCHEDULED timestamp as ISO 8601 date string (no time if all-day).
    pub scheduled: Option<String>,
    /// DEADLINE timestamp as ISO 8601 date string.
    pub deadline: Option<String>,
    /// CLOSED timestamp as RFC3339 (with time if present).
    pub closed: Option<String>,
    /// Raw repeater cookie from SCHEDULED (e.g. "+1w", ".+1d", "++1m").
    pub recurrence_cookie: Option<String>,
    /// End time of a time-range (HH:MM), if SCHEDULED or DEADLINE had one.
    pub time_range_end: Option<String>,
    /// Body text (non-drawer, non-planning lines under the heading).
    pub body: String,
    /// `:ID:` property value (empty if none).
    pub id_property: String,
    /// All PROPERTIES drawer key/value pairs.
    pub properties: BTreeMap<String, String>,
    /// CLOCK entries from the LOGBOOK drawer.
    pub clock_entries: Vec<String>,
    /// State change entries from the LOGBOOK (e.g. "State DONE from TODO [ts]").
    pub state_changes: Vec<String>,
    /// 1-based index of this heading among all headings in the file (used for
    /// the position-hash guid fallback).
    pub position_index: usize,
    /// The org file this task came from (relative to the watch folder).
    pub file_rel: String,
    /// File's last-modified time as RFC3339 (used to partition the raw layer).
    pub file_mtime: String,
}

/// Parse result from an org timestamp.
#[derive(Debug, Default)]
struct OrgTsParsed {
    /// ISO date "YYYY-MM-DD" or RFC3339 with time.
    value: String,
    /// Repeater/warning cookie: "+1w", ".+2d", "++1m", "-3d", etc.
    repeater: Option<String>,
    /// End time of a time-range "HH:MM", present when the timestamp had "HH:MM-HH:MM".
    range_end: Option<String>,
}

/// Parse an org timestamp string such as:
/// - `<2024-01-15 Mon>` (active, all-day)
/// - `<2024-01-15 Mon 14:00>` (active, with time)
/// - `<2024-01-15 Mon 14:00-15:30>` (time range)
/// - `<2024-06-20 Sat 12:30 +1w>` (timed recurring)
/// - `[2024-01-15 Mon]` (inactive)
///
/// Returns `None` on parse failure.
fn parse_org_ts_full(s: &str) -> Option<OrgTsParsed> {
    let inner = s
        .trim_start_matches(|c| c == '<' || c == '[')
        .trim_end_matches(|c| c == '>' || c == ']')
        .trim();
    // Split on whitespace; first token is the date, second is day-of-week,
    // remaining tokens may be: time ("HH:MM" or "HH:MM-HH:MM"), repeater (+1w),
    // warning cookie (-3d), or combinations thereof.
    let tokens: Vec<&str> = inner.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let date_str = tokens[0];
    if date_str.len() < 10 {
        return None;
    }
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;

    // Scan tokens[2..] for a time and a repeater/warning cookie.
    // Time tokens look like "HH:MM" or "HH:MM-HH:MM".
    // Cookie tokens start with '+', '.', or '-' followed by digits (e.g. +1w, .+2d, ++1m, -3d).
    let is_repeater = |t: &str| -> bool {
        t.starts_with("++") || t.starts_with(".+") ||
        (t.starts_with('+') && t.len() > 1 && t[1..].starts_with(|c: char| c.is_ascii_digit())) ||
        (t.starts_with('-') && t.len() > 1 && t[1..].starts_with(|c: char| c.is_ascii_digit()))
    };
    let is_time = |t: &str| -> bool {
        // "HH:MM" or "HH:MM-HH:MM"
        let start = t.split('-').next().unwrap_or("");
        start.len() == 5 && start.chars().nth(2) == Some(':')
    };

    let mut time_start: Option<&str> = None;
    let mut time_end: Option<String> = None;
    let mut repeater: Option<String> = None;

    for &tok in tokens.get(2..).unwrap_or(&[]) {
        if is_time(tok) {
            // Could be "HH:MM" or "HH:MM-HH:MM"
            let parts: Vec<&str> = tok.splitn(2, '-').collect();
            time_start = Some(parts[0]);
            if parts.len() == 2 && parts[1].len() == 5 && parts[1].chars().nth(2) == Some(':') {
                time_end = Some(parts[1].to_string());
            }
        } else if is_repeater(tok) {
            repeater = Some(tok.to_string());
        }
    }

    if let Some(hhmm) = time_start {
        let combined = format!("{date_str}T{hhmm}:00");
        if let Ok(ndt) = NaiveDateTime::parse_from_str(&combined, "%Y-%m-%dT%H:%M:%S") {
            if let Some(local) = Local.from_local_datetime(&ndt).single() {
                return Some(OrgTsParsed {
                    value: local.to_rfc3339(),
                    repeater,
                    range_end: time_end,
                });
            }
        }
    }

    // All-day.
    Some(OrgTsParsed {
        value: date_str.to_string(),
        repeater,
        range_end: None,
    })
}

/// Simplified timestamp parse — returns an ISO string (date or RFC3339).
/// Kept for CLOSED timestamps where we don't need repeater info.
fn parse_org_ts(s: &str) -> Option<String> {
    parse_org_ts_full(s).map(|r| r.value)
}

/// Result of parsing a planning line, carrying full timestamp details.
struct PlanningLine {
    scheduled: Option<OrgTsParsed>,
    deadline: Option<OrgTsParsed>,
    closed: Option<String>,
}

/// Extract the planning keywords (SCHEDULED/DEADLINE/CLOSED) from a planning line.
fn parse_planning_line(line: &str) -> PlanningLine {
    let mut scheduled = None;
    let mut deadline = None;
    let mut closed = None;

    // Pattern: KEYWORD: <timestamp> or KEYWORD: [timestamp]
    let mut rest = line;
    while !rest.is_empty() {
        if let Some(pos) = rest.find("SCHEDULED:") {
            let after = rest[pos + 10..].trim_start();
            if let Some(end) = after.find(|c| c == '>' || c == ']') {
                let ts_str = &after[..end + 1];
                scheduled = parse_org_ts_full(ts_str);
            }
        }
        if let Some(pos) = rest.find("DEADLINE:") {
            let after = rest[pos + 9..].trim_start();
            if let Some(end) = after.find(|c| c == '>' || c == ']') {
                let ts_str = &after[..end + 1];
                deadline = parse_org_ts_full(ts_str);
            }
        }
        if let Some(pos) = rest.find("CLOSED:") {
            let after = rest[pos + 7..].trim_start();
            if let Some(end) = after.find(|c| c == '>' || c == ']') {
                let ts_str = &after[..end + 1];
                closed = parse_org_ts(ts_str);
            }
        }
        // advance past first keyword to avoid re-scanning
        if let Some(p) = rest.find(':') {
            rest = &rest[p + 1..];
        } else {
            break;
        }
    }
    PlanningLine { scheduled, deadline, closed }
}

/// Parse a heading line into its components, using file-local keyword sets for classification.
/// Format: `STARS [KEYWORD] [[#P]] TITLE [:tags:]`
/// Returns `None` if the line doesn't start with `*` or has no TODO keyword.
fn parse_heading_with_file_kws(
    line: &str,
    file_open: &HashSet<String>,
    file_done: &HashSet<String>,
) -> Option<(usize, TodoState, String, String, Vec<String>)> {
    // Count leading asterisks.
    let stars = line.chars().take_while(|&c| c == '*').count();
    if stars == 0 {
        return None;
    }
    let rest = line.get(stars..)?.trim_start();
    if rest.is_empty() {
        return None;
    }

    // Optional TODO keyword (all-caps word, may include '-').
    let mut rest_after_kw = rest;
    let mut state_opt: Option<TodoState> = None;
    {
        let first_word: &str = rest.split_whitespace().next().unwrap_or("");
        if first_word.chars().all(|c| c.is_ascii_uppercase() || c == '-') && !first_word.is_empty()
        {
            if let Some(st) = classify_keyword_with_file_kws(first_word, file_open, file_done) {
                state_opt = Some(st);
                rest_after_kw = rest[first_word.len()..].trim_start();
            }
        }
    }
    // Must have a keyword to be a task heading.
    let state = state_opt?;

    // Optional priority cookie: [#A]
    let (priority, rest_after_prio) = if rest_after_kw.starts_with("[#") {
        if let Some(end) = rest_after_kw.find(']') {
            let prio = rest_after_kw[2..end].trim().to_string();
            let after = rest_after_kw[end + 1..].trim_start();
            (prio, after)
        } else {
            (String::new(), rest_after_kw)
        }
    } else {
        (String::new(), rest_after_kw)
    };

    // Tags at end of line: `:tag1:tag2:`
    // Tags appear as a trailing colon-delimited group preceded by whitespace.
    let (title_raw, tags) = if let Some(idx) = rest_after_prio.rfind(" :") {
        let tag_part = rest_after_prio[idx + 1..].trim();
        if tag_part.starts_with(':') && tag_part.ends_with(':') && tag_part.len() > 2 {
            let tags: Vec<String> = tag_part[1..tag_part.len() - 1]
                .split(':')
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect();
            (rest_after_prio[..idx].trim().to_string(), tags)
        } else {
            (rest_after_prio.trim().to_string(), Vec::new())
        }
    } else {
        (rest_after_prio.trim().to_string(), Vec::new())
    };

    Some((stars, state, priority, title_raw, tags))
}

/// Convenience wrapper for parse_heading_with_file_kws with empty file keyword sets
/// (falls back entirely to built-in keywords).
#[allow(dead_code)]
fn parse_heading(line: &str) -> Option<(usize, TodoState, String, String, Vec<String>)> {
    parse_heading_with_file_kws(line, &HashSet::new(), &HashSet::new())
}

/// Derive a stable GUID for an org heading:
/// 1. If `:ID:` property is set — use it (stable across renames).
/// 2. Otherwise — SHA-256 of `"<file_rel>|<heading_text>|<position_index>"`,
///    hex-encoded, truncated to 16 chars.
fn derive_guid(id_property: &str, file_rel: &str, title: &str, position_index: usize) -> String {
    if !id_property.is_empty() {
        return format!("org:{id_property}");
    }
    let input = format!("{file_rel}|{title}|{position_index}");
    let hash = Sha256::digest(input.as_bytes());
    format!("org-hash:{}", hex::encode(&hash[..8]))
}

/// Parse #+TODO: / #+SEQ_TODO: / #+TYP_TODO: directives from file content.
/// Returns two sets: open keywords and done keywords.
/// Tokens before `|` are open; tokens after `|` (or the last token if no `|`)
/// are done. Fast-access keys like `TODO(t)` are stripped to bare `TODO`.
fn parse_file_todo_keywords(content: &str) -> (HashSet<String>, HashSet<String>) {
    let mut open: HashSet<String> = HashSet::new();
    let mut done: HashSet<String> = HashSet::new();

    // Strip a fast-access key suffix like "(t)" from a keyword token.
    let strip_fast = |tok: &str| -> String {
        if let Some(pos) = tok.find('(') {
            tok[..pos].trim().to_string()
        } else {
            tok.trim().to_string()
        }
    };

    for line in content.lines() {
        let trimmed = line.trim();
        let directive = if trimmed.to_ascii_uppercase().starts_with("#+TODO:") {
            Some(&trimmed[7..])
        } else if trimmed.to_ascii_uppercase().starts_with("#+SEQ_TODO:") {
            Some(&trimmed[11..])
        } else if trimmed.to_ascii_uppercase().starts_with("#+TYP_TODO:") {
            Some(&trimmed[11..])
        } else {
            None
        };

        if let Some(rest) = directive {
            // Split on `|`: left side = open, right side = done.
            let (open_part, done_part) = if let Some(pipe) = rest.find('|') {
                (&rest[..pipe], &rest[pipe + 1..])
            } else {
                // No pipe: all but the last token are open; the last is done.
                let tokens: Vec<&str> = rest.split_whitespace().collect();
                if tokens.len() <= 1 {
                    (rest, "")
                } else {
                    let split = rest.rfind(tokens[tokens.len() - 1]).unwrap_or(rest.len());
                    (&rest[..split], &rest[split..])
                }
            };
            for tok in open_part.split_whitespace() {
                let kw = strip_fast(tok);
                if !kw.is_empty() {
                    open.insert(kw);
                }
            }
            for tok in done_part.split_whitespace() {
                let kw = strip_fast(tok);
                if !kw.is_empty() {
                    done.insert(kw);
                }
            }
        }
    }

    (open, done)
}

/// Classify a keyword using the file's own keyword sets (merged with built-ins).
fn classify_keyword_with_file_kws(
    kw: &str,
    file_open: &HashSet<String>,
    file_done: &HashSet<String>,
) -> Option<TodoState> {
    // Check file-local keywords first (they override or supplement the defaults).
    if file_done.contains(kw) {
        return Some(TodoState::Done(kw.to_string()));
    }
    if file_open.contains(kw) {
        return Some(TodoState::Open(kw.to_string()));
    }
    // Fall back to the built-in defaults.
    classify_keyword(kw)
}

/// Parse one `.org` file into a list of [`OrgTask`]s. Lenient: headings
/// without a TODO keyword are skipped; malformed timestamps fall through
/// gracefully.
pub(crate) fn parse_org_file(content: &str, file_rel: &str, file_mtime: &str) -> Vec<OrgTask> {
    let mut tasks: Vec<OrgTask> = Vec::new();
    let mut position_index: usize = 0;

    // Parse per-file #+TODO: directives before scanning headings.
    let (file_open_kws, file_done_kws) = parse_file_todo_keywords(content);

    // We process line by line, tracking the "current heading" and its body.
    enum BodySection {
        Planning,
        Properties,
        Logbook,
        Body,
    }

    // State for current heading.
    struct HeadingAccum {
        level: usize,
        state: TodoState,
        priority: String,
        title: String,
        tags: Vec<String>,
        scheduled: Option<String>,
        deadline: Option<String>,
        closed: Option<String>,
        recurrence_cookie: Option<String>,
        time_range_end: Option<String>,
        body_lines: Vec<String>,
        id_property: String,
        properties: BTreeMap<String, String>,
        clock_entries: Vec<String>,
        state_changes: Vec<String>,
        position_index: usize,
        section: BodySection,
    }

    let mut current: Option<HeadingAccum> = None;

    let flush = |acc: HeadingAccum, tasks: &mut Vec<OrgTask>, file_rel: &str, file_mtime: &str| {
        let guid = derive_guid(&acc.id_property, file_rel, &acc.title, acc.position_index);
        tasks.push(OrgTask {
            guid,
            level: acc.level,
            keyword: acc.state.keyword().to_string(),
            done: acc.state.is_done(),
            priority: acc.priority,
            title: acc.title,
            tags: acc.tags,
            scheduled: acc.scheduled,
            deadline: acc.deadline,
            closed: acc.closed,
            recurrence_cookie: acc.recurrence_cookie,
            time_range_end: acc.time_range_end,
            body: acc.body_lines.join("\n"),
            id_property: acc.id_property,
            properties: acc.properties,
            clock_entries: acc.clock_entries,
            state_changes: acc.state_changes,
            position_index: acc.position_index,
            file_rel: file_rel.to_string(),
            file_mtime: file_mtime.to_string(),
        });
    };

    for raw_line in content.lines() {
        let line = raw_line.trim_end();

        // Is this a heading line?
        if line.starts_with('*') {
            if let Some((level, state, priority, title, tags)) =
                parse_heading_with_file_kws(line, &file_open_kws, &file_done_kws)
            {
                // Flush the previous task.
                if let Some(acc) = current.take() {
                    flush(acc, &mut tasks, file_rel, file_mtime);
                }
                position_index += 1;
                current = Some(HeadingAccum {
                    level,
                    state,
                    priority,
                    title,
                    tags,
                    scheduled: None,
                    deadline: None,
                    closed: None,
                    recurrence_cookie: None,
                    time_range_end: None,
                    body_lines: Vec::new(),
                    id_property: String::new(),
                    properties: BTreeMap::new(),
                    clock_entries: Vec::new(),
                    state_changes: Vec::new(),
                    position_index,
                    section: BodySection::Planning,
                });
                continue;
            } else {
                // A heading line with no TODO keyword — not a task; flush any open task.
                if let Some(acc) = current.take() {
                    flush(acc, &mut tasks, file_rel, file_mtime);
                }
                continue;
            }
        }

        // Lines below a task heading.
        let Some(ref mut acc) = current else {
            continue;
        };

        let trimmed = line.trim();

        // Drawer markers.
        if trimmed.eq_ignore_ascii_case(":PROPERTIES:") {
            acc.section = BodySection::Properties;
            continue;
        }
        if trimmed.eq_ignore_ascii_case(":LOGBOOK:") {
            acc.section = BodySection::Logbook;
            continue;
        }
        if trimmed.eq_ignore_ascii_case(":END:") {
            acc.section = BodySection::Body;
            continue;
        }

        match acc.section {
            BodySection::Planning => {
                // Planning lines: SCHEDULED/DEADLINE/CLOSED keywords.
                if trimmed.contains("SCHEDULED:")
                    || trimmed.contains("DEADLINE:")
                    || trimmed.contains("CLOSED:")
                {
                    let pl = parse_planning_line(trimmed);
                    if let Some(sc) = pl.scheduled {
                        // Pick up the repeater from whichever timestamp carries it.
                        if acc.recurrence_cookie.is_none() {
                            acc.recurrence_cookie = sc.repeater.clone();
                        }
                        if acc.time_range_end.is_none() {
                            acc.time_range_end = sc.range_end.clone();
                        }
                        acc.scheduled = Some(sc.value);
                    }
                    if let Some(dl) = pl.deadline {
                        if acc.recurrence_cookie.is_none() {
                            acc.recurrence_cookie = dl.repeater.clone();
                        }
                        if acc.time_range_end.is_none() {
                            acc.time_range_end = dl.range_end.clone();
                        }
                        acc.deadline = Some(dl.value);
                    }
                    if pl.closed.is_some() {
                        acc.closed = pl.closed;
                    }
                    continue;
                }
                // A blank line or non-planning line means body follows.
                if !trimmed.is_empty() {
                    acc.section = BodySection::Body;
                    acc.body_lines.push(line.to_string());
                }
            }
            BodySection::Properties => {
                // `:KEY: value`
                if let Some(rest) = trimmed.strip_prefix(':') {
                    if let Some(colon) = rest.find(':') {
                        let key = rest[..colon].trim().to_string();
                        let val = rest[colon + 1..].trim().to_string();
                        if key.eq_ignore_ascii_case("ID") {
                            acc.id_property = val.clone();
                        }
                        acc.properties.insert(key, val);
                    }
                }
            }
            BodySection::Logbook => {
                // CLOCK entries: `CLOCK: [ts]--[ts] => HH:MM`
                if trimmed.to_ascii_uppercase().starts_with("CLOCK:") {
                    acc.clock_entries.push(trimmed.to_string());
                } else if trimmed.contains("State ") {
                    // `- State "DONE" from "TODO" [ts]`
                    acc.state_changes.push(trimmed.to_string());
                }
            }
            BodySection::Body => {
                acc.body_lines.push(line.to_string());
            }
        }
    }

    // Flush the last task.
    if let Some(acc) = current.take() {
        flush(acc, &mut tasks, file_rel, file_mtime);
    }

    tasks
}

// ---------------------------------------------------------------------------
// File mtime helper.

fn file_mtime_rfc3339(path: &Path) -> String {
    use std::time::UNIX_EPOCH;
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| {
            DateTime::from_timestamp(d.as_secs() as i64, 0)
                .map(|utc| utc.with_timezone(&Local).to_rfc3339())
        })
        .unwrap_or_else(|| Local::now().to_rfc3339())
}

// ---------------------------------------------------------------------------
// Scan an org folder.

/// Walk a directory for `.org` files (recursive, up to 8 levels deep).
fn collect_org_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 8 {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, depth + 1, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("org") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Raw layer upsert.

/// One raw record: the full org task, stored at full fidelity in
/// `tasks/org-mode/raw/YYYY-MM.jsonl`, keyed by `guid`, partitioned by
/// `file_mtime` month.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawOrgTask {
    guid: String,
    keyword: String,
    done: bool,
    priority: String,
    title: String,
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheduled: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deadline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recurrence_cookie: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time_range_end: Option<String>,
    body: String,
    id_property: String,
    properties: BTreeMap<String, String>,
    clock_entries: Vec<String>,
    state_changes: Vec<String>,
    position_index: usize,
    file_rel: String,
    file_mtime: String,
}

impl From<&OrgTask> for RawOrgTask {
    fn from(t: &OrgTask) -> Self {
        RawOrgTask {
            guid: t.guid.clone(),
            keyword: t.keyword.clone(),
            done: t.done,
            priority: t.priority.clone(),
            title: t.title.clone(),
            tags: t.tags.clone(),
            scheduled: t.scheduled.clone(),
            deadline: t.deadline.clone(),
            closed: t.closed.clone(),
            recurrence_cookie: t.recurrence_cookie.clone(),
            time_range_end: t.time_range_end.clone(),
            body: t.body.clone(),
            id_property: t.id_property.clone(),
            properties: t.properties.clone(),
            clock_entries: t.clock_entries.clone(),
            state_changes: t.state_changes.clone(),
            position_index: t.position_index,
            file_rel: t.file_rel.clone(),
            file_mtime: t.file_mtime.clone(),
        }
    }
}

/// Upsert raw rows into `tasks/org-mode/raw/YYYY-MM.jsonl`, keyed by guid.
/// Returns the count of newly-inserted (vs. updated) rows.
fn upsert_raw(vault: &Vault, rows: Vec<RawOrgTask>) -> Result<u64> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawOrgTask>> = BTreeMap::new();
    for r in rows {
        let key = Partition::Month
            .key(&r.file_mtime)
            .with_context(|| format!("org-mode: raw task file_mtime {:?} has no month", r.file_mtime))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<RawOrgTask> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, r)| (r.guid.clone(), i))
            .collect();
        for r in fresh {
            match idx.get(&r.guid).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(r.guid.clone(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| a.file_mtime.cmp(&b.file_mtime).then_with(|| a.guid.cmp(&b.guid)));
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// Normalize an OrgTask → Task (contract row).

/// Priority cookie → TickTick-compatible priority scale: 5=high, 3=medium,
/// 1=low, 0=none. Org uses A=highest, B=medium, C=low.
fn priority_to_int(prio: &str) -> i64 {
    match prio {
        "A" | "1" => 5,
        "B" | "2" => 3,
        "C" | "3" => 1,
        _ if !prio.is_empty() => 1, // any other letter → low
        _ => 0,
    }
}

/// Convert a repeater cookie string ("+1w", ".+2d", "++1m") to a minimal RRULE string.
/// Returns None for unrecognized formats; callers can fall back to storing the raw cookie.
fn cookie_to_rrule(cookie: &str) -> Option<String> {
    // Strip leading +, .+, or ++ to get the interval+unit.
    let stripped = cookie.trim_start_matches('.').trim_start_matches('+');
    if stripped.is_empty() {
        return None;
    }
    let unit_char = stripped.chars().last()?;
    let interval_str = &stripped[..stripped.len() - unit_char.len_utf8()];
    let interval: u32 = interval_str.parse().ok()?;
    let freq = match unit_char {
        'd' => "DAILY",
        'w' => "WEEKLY",
        'm' => "MONTHLY",
        'y' => "YEARLY",
        _ => return None,
    };
    Some(format!("FREQ={freq};INTERVAL={interval}"))
}

/// Convert an [`OrgTask`] into the normalized [`Task`] contract row.
fn to_task(org: &OrgTask) -> Task {
    let mut extra: Map<String, Value> = Map::new();
    if !org.id_property.is_empty() {
        extra.insert("org_id_property".into(), Value::String(org.id_property.clone()));
    }
    if !org.clock_entries.is_empty() {
        extra.insert(
            "clock_entries".into(),
            Value::Array(org.clock_entries.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if !org.state_changes.is_empty() {
        extra.insert(
            "state_changes".into(),
            Value::Array(org.state_changes.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    // Preserve the raw repeater cookie for tools that want the original form.
    if let Some(cookie) = &org.recurrence_cookie {
        extra.insert("org_recurrence_cookie".into(), Value::String(cookie.clone()));
    }
    // Preserve time-range end for full fidelity (e.g. 10:00-12:00 → extra.time_range_end="12:00").
    if let Some(end_time) = &org.time_range_end {
        extra.insert("time_range_end".into(), Value::String(end_time.clone()));
    }
    for (k, v) in &org.properties {
        if k.eq_ignore_ascii_case("ID") {
            continue; // already in guid/org_id_property
        }
        extra.insert(format!("prop_{k}"), Value::String(v.clone()));
    }
    extra.insert("org_level".into(), Value::Number(org.level.into()));
    extra.insert("org_file".into(), Value::String(org.file_rel.clone()));
    extra.insert("org_keyword".into(), Value::String(org.keyword.clone()));

    // Recurrence: try to map the cookie to RRULE; fall back to raw cookie string.
    let recurrence = org.recurrence_cookie.as_deref().and_then(|c| {
        cookie_to_rrule(c).or_else(|| Some(c.to_string()))
    });

    // due = DEADLINE only (org semantics: DEADLINE is a due date, SCHEDULED is start).
    let due = org.deadline.clone();
    // start = SCHEDULED (begin date / planned start).
    let start = org.scheduled.clone();

    // all_day: true only if the relevant timestamp is a bare date (no time component).
    // Check whichever timestamp is set (prefer deadline for due, scheduled for start).
    let all_day = due
        .as_deref()
        .or(start.as_deref())
        .map(|s| s.len() == 10) // "YYYY-MM-DD" = all-day; RFC3339 has time
        .unwrap_or(false);

    // created: from :CREATED: property (common convention in org-mode).
    let created = org.properties.get("CREATED").and_then(|v| {
        // Could be a plain date or an org timestamp; try to parse it.
        if v.len() == 10 {
            Some(v.clone()) // bare date
        } else {
            // Try parsing as org timestamp (may have surrounding brackets).
            parse_org_ts(v).or_else(|| if !v.is_empty() { Some(v.clone()) } else { None })
        }
    });

    // notes: body text maps to the tasks contract's first-class notes field.
    let notes = org.body.trim().to_string();

    Task {
        source: SOURCE.to_string(),
        id: org.guid.clone(),
        title: org.title.clone(),
        project: String::new(), // org-mode has no native "project" concept
        notes,
        status: if org.done { "done".into() } else { "open".into() },
        priority: priority_to_int(&org.priority),
        due,
        start,
        all_day,
        recurrence,
        tags: org.tags.clone(),
        subtasks: Vec::new(),
        created,
        modified: Some(org.file_mtime.clone()),
        completed: org.closed.clone(),
        extra,
    }
}

// ---------------------------------------------------------------------------
// Stats.

#[derive(Debug, Default)]
struct SyncStats {
    pub open: u64,
    pub created: u64,
    pub completed: u64,
    pub deleted: u64,
}

// ---------------------------------------------------------------------------
// The collect pass.

/// One full scan of the configured org folder.
fn collect_org(vault: &Vault) -> Result<SyncStats> {
    let config = vault.read_org_config();
    if config.folder.trim().is_empty() {
        // Not configured — silent no-op, not an error.
        return Ok(SyncStats::default());
    }
    let root = PathBuf::from(&config.folder);
    if !root.exists() {
        // Folder not reachable — silent no-op (could be unmounted volume).
        return Ok(SyncStats::default());
    }

    let org_files = collect_org_files(&root);

    // Parse every .org file and collect all tasks.
    // Build two indices: guid→task for fate resolution, and file_rel set.
    let mut all_org_tasks: Vec<OrgTask> = Vec::new();
    let mut files_present: HashSet<String> = HashSet::new();
    let mut all_raw: Vec<RawOrgTask> = Vec::new();

    for path in &org_files {
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        files_present.insert(rel.clone());

        let mtime = file_mtime_rfc3339(path);
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("trove org-mode: skipping {:?}: {e}", path);
                continue;
            }
        };
        let parsed = parse_org_file(&content, &rel, &mtime);
        for t in &parsed {
            all_raw.push(RawOrgTask::from(t));
        }
        all_org_tasks.extend(parsed);
    }

    // Raw layer: upsert all tasks.
    let _raw_new = if !all_raw.is_empty() { upsert_raw(vault, all_raw)? } else { 0 };

    // Contract layer: open tasks only → fresh list for apply_tasks_sync.
    // Done tasks are handled via the fate closure.
    let fresh: Vec<Task> = all_org_tasks
        .iter()
        .filter(|t| !t.done)
        .map(to_task)
        .collect();

    // Build a guid→OrgTask map for fate resolution (covers all tasks, open+done).
    let guid_to_org: HashMap<String, &OrgTask> =
        all_org_tasks.iter().map(|t| (t.guid.clone(), t)).collect();
    // File path → set of guids: which tasks live in which file.
    let mut file_to_guids: HashMap<String, HashSet<String>> = HashMap::new();
    for t in &all_org_tasks {
        file_to_guids.entry(t.file_rel.clone()).or_default().insert(t.guid.clone());
    }

    // Projects: org-mode has no project concept; use a single placeholder project.
    let projects: Vec<ProjectInfo> =
        vec![ProjectInfo { id: "org-mode".into(), name: "Org-mode".into() }];

    // Fate closure: for tasks that disappeared from the open snapshot.
    let stats = vault.apply_tasks_sync(SOURCE, &projects, fresh, |prev_task: &Task| {
        let guid = &prev_task.id;
        // If the guid is now present as a DONE task → Completed.
        if let Some(org) = guid_to_org.get(guid) {
            if org.done {
                return TaskFate::Completed(org.closed.clone());
            }
            // Still open but missing from fresh? Shouldn't happen (we include all
            // non-done tasks in fresh). Carry forward.
            return TaskFate::Unknown;
        }
        // The guid is not found in any current file.
        // Determine the file from the extra.org_file field.
        let file_rel = prev_task
            .extra
            .get("org_file")
            .and_then(Value::as_str)
            .unwrap_or("");
        if file_rel.is_empty() || !files_present.contains(file_rel) {
            // File is gone or we don't know which file it was in → carry forward.
            return TaskFate::Unknown;
        }
        // File is still present but the task heading is gone → deleted.
        TaskFate::Deleted
    })?;

    Ok(SyncStats {
        open: stats.open,
        created: stats.created,
        completed: stats.completed,
        deleted: stats.deleted,
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-org-mode-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Parser unit tests — built from the org-syntax spec and manual examples.

    /// Simple TODO heading, no extras.
    #[test]
    fn parse_bare_todo() {
        let content = "* TODO Buy groceries\n";
        let tasks = parse_org_file(content, "test.org", "2026-06-01T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.keyword, "TODO");
        assert!(!t.done);
        assert_eq!(t.title, "Buy groceries");
        assert!(t.tags.is_empty());
        assert_eq!(t.priority, "");
    }

    /// DONE task with CLOSED timestamp.
    #[test]
    fn parse_done_task_with_closed() {
        let content = "\
* DONE [#A] Fix critical bug :work:urgent:
  CLOSED: [2026-06-15 Mon 14:30]
";
        let tasks = parse_org_file(content, "test.org", "2026-06-15T20:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.keyword, "DONE");
        assert!(t.done);
        assert_eq!(t.priority, "A");
        assert_eq!(t.title, "Fix critical bug");
        assert_eq!(t.tags, vec!["work", "urgent"]);
        assert!(t.closed.is_some(), "CLOSED should be parsed");
    }

    /// SCHEDULED and DEADLINE timestamps.
    #[test]
    fn parse_scheduled_deadline() {
        let content = "\
* TODO Plan the sprint :team:
  SCHEDULED: <2026-06-20 Sat>  DEADLINE: <2026-06-25 Thu>
";
        let tasks = parse_org_file(content, "sprint.org", "2026-06-15T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.scheduled.as_deref(), Some("2026-06-20"));
        assert_eq!(t.deadline.as_deref(), Some("2026-06-25"));
    }

    /// Timestamp with time component.
    #[test]
    fn parse_timestamp_with_time() {
        let ts = parse_org_ts("<2026-06-15 Mon 14:00>");
        assert!(ts.is_some());
        let s = ts.unwrap();
        // Should be RFC3339 (longer than YYYY-MM-DD).
        assert!(s.len() > 10, "got: {s}");
        assert!(s.contains("14:"), "time should be present: {s}");
    }

    /// All-day timestamp.
    #[test]
    fn parse_timestamp_allday() {
        let ts = parse_org_ts("<2026-06-15 Mon>");
        assert_eq!(ts.as_deref(), Some("2026-06-15"));
    }

    /// Inactive timestamp.
    #[test]
    fn parse_inactive_timestamp() {
        let ts = parse_org_ts("[2026-06-15 Mon]");
        assert_eq!(ts.as_deref(), Some("2026-06-15"));
    }

    /// :PROPERTIES: drawer with :ID: key.
    #[test]
    fn parse_id_property() {
        let content = "\
* TODO Write release notes
  :PROPERTIES:
  :ID: abc-123-def
  :CREATED: 2026-01-01
  :END:
";
        let tasks = parse_org_file(content, "notes.org", "2026-06-01T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.id_property, "abc-123-def");
        assert_eq!(t.guid, "org:abc-123-def");
        assert!(t.properties.contains_key("CREATED"));
    }

    /// Position-hash guid when no :ID: property is set.
    #[test]
    fn guid_position_hash_no_id() {
        let content = "* TODO Task without ID\n";
        let tasks = parse_org_file(content, "file.org", "2026-06-01T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let guid = &tasks[0].guid;
        assert!(
            guid.starts_with("org-hash:"),
            "fallback guid should be a hash: {guid}"
        );
    }

    /// Two headings at different levels; only task headings (with TODO keyword) yield tasks.
    #[test]
    fn parse_multi_heading_with_non_task() {
        let content = "\
* Projects
** TODO Write docs
** A plain heading with no TODO
** DONE Ship the feature :deploy:
   CLOSED: [2026-06-14 Sun 09:00]
";
        let tasks = parse_org_file(content, "work.org", "2026-06-14T15:00:00+00:00");
        // Only TODO and DONE headings yield tasks; the plain heading and the section header do not.
        assert_eq!(tasks.len(), 2, "got: {tasks:?}");
        assert_eq!(tasks[0].title, "Write docs");
        assert_eq!(tasks[1].title, "Ship the feature");
        assert!(tasks[1].done);
    }

    /// :LOGBOOK: drawer: CLOCK entries and state changes are captured.
    #[test]
    fn parse_logbook_drawer() {
        let content = "\
* DONE Review PR :dev:
  CLOSED: [2026-06-15 Mon 11:30]
  :LOGBOOK:
  CLOCK: [2026-06-15 Mon 10:00]--[2026-06-15 Mon 11:30] =>  1:30
  - State \"DONE\" from \"TODO\" [2026-06-15 Mon 11:30]
  :END:
";
        let tasks = parse_org_file(content, "dev.org", "2026-06-15T12:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.clock_entries.len(), 1);
        assert!(t.clock_entries[0].starts_with("CLOCK:"));
        assert_eq!(t.state_changes.len(), 1);
        assert!(t.state_changes[0].contains("DONE"));
    }

    /// Different WAITING/NEXT/SOMEDAY keywords are treated as open tasks.
    #[test]
    fn parse_various_open_keywords() {
        let content = "\
* WAITING Approval from team
* NEXT Send invoice
* SOMEDAY Learn Haskell
";
        let tasks = parse_org_file(content, "misc.org", "2026-06-01T10:00:00+00:00");
        assert_eq!(tasks.len(), 3);
        assert!(tasks.iter().all(|t| !t.done));
    }

    /// Idempotent re-parse: same content yields same guids.
    #[test]
    fn idempotent_guid() {
        let content = "* TODO Idempotent task\n";
        let a = parse_org_file(content, "f.org", "2026-06-01T00:00:00+00:00");
        let b = parse_org_file(content, "f.org", "2026-06-01T00:00:00+00:00");
        assert_eq!(a[0].guid, b[0].guid);
    }

    // -----------------------------------------------------------------------
    // Priority mapping.

    #[test]
    fn priority_mapping() {
        assert_eq!(priority_to_int("A"), 5);
        assert_eq!(priority_to_int("B"), 3);
        assert_eq!(priority_to_int("C"), 1);
        assert_eq!(priority_to_int(""), 0);
        assert_eq!(priority_to_int("D"), 1); // unknown letter → low
    }

    // -----------------------------------------------------------------------
    // Contract integration: apply_tasks_sync with the org fate closure.

    /// A first sync of two open tasks; both appear as "created" in the event stream.
    #[test]
    fn first_sync_creates_tasks() -> Result<()> {
        let vault = temp_vault("first_sync");

        // Write a temp org folder with one file.
        let org_dir = std::env::temp_dir()
            .join(format!("trove-org-mode-orgdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&org_dir);
        fs::create_dir_all(&org_dir)?;
        fs::write(
            org_dir.join("tasks.org"),
            "* TODO Write tests\n* TODO Ship it\n",
        )?;

        // Write config.
        let config_path = vault.resolve(CONFIG_FILE)?;
        if let Some(p) = config_path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(
            &config_path,
            serde_json::to_string(&OrgConfig { folder: org_dir.to_string_lossy().into() })?,
        )?;

        // Run collect.
        let stats = collect_org(&vault)?;
        assert_eq!(stats.open, 2);
        assert_eq!(stats.created, 2);

        // Snapshot was written.
        let snap = vault.load_tasks_snapshot(SOURCE)?;
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|t| t.source == SOURCE));

        let _ = fs::remove_dir_all(&org_dir);
        Ok(())
    }

    /// Second sync: a task completed (DONE) in the file → completion event logged.
    #[test]
    fn second_sync_detects_completion() -> Result<()> {
        let vault = temp_vault("second_sync");
        let org_dir = std::env::temp_dir()
            .join(format!("trove-org-mode-orgdir2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&org_dir);
        fs::create_dir_all(&org_dir)?;

        // First sync: both tasks open.
        fs::write(
            org_dir.join("tasks.org"),
            "* TODO Task A\n  :PROPERTIES:\n  :ID: id-a\n  :END:\n* TODO Task B\n  :PROPERTIES:\n  :ID: id-b\n  :END:\n",
        )?;
        let config_path = vault.resolve(CONFIG_FILE)?;
        if let Some(p) = config_path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(
            &config_path,
            serde_json::to_string(&OrgConfig { folder: org_dir.to_string_lossy().into() })?,
        )?;
        let s1 = collect_org(&vault)?;
        assert_eq!(s1.open, 2);

        // Second sync: task A is now DONE.
        fs::write(
            org_dir.join("tasks.org"),
            "* DONE Task A\n  CLOSED: [2026-06-15 Mon 10:00]\n  :PROPERTIES:\n  :ID: id-a\n  :END:\n* TODO Task B\n  :PROPERTIES:\n  :ID: id-b\n  :END:\n",
        )?;
        let s2 = collect_org(&vault)?;
        assert_eq!(s2.open, 1, "only task B remains open");
        assert_eq!(s2.completed, 1, "task A completed");

        let snap = vault.load_tasks_snapshot(SOURCE)?;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].title, "Task B");

        let _ = fs::remove_dir_all(&org_dir);
        Ok(())
    }

    /// Re-sync of the same content is idempotent: no new events, snapshot unchanged.
    #[test]
    fn idempotent_re_sync() -> Result<()> {
        let vault = temp_vault("idempotent");
        let org_dir = std::env::temp_dir()
            .join(format!("trove-org-mode-orgdir3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&org_dir);
        fs::create_dir_all(&org_dir)?;
        fs::write(
            org_dir.join("tasks.org"),
            "* TODO Stable task\n  :PROPERTIES:\n  :ID: stable-1\n  :END:\n",
        )?;
        let config_path = vault.resolve(CONFIG_FILE)?;
        if let Some(p) = config_path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(
            &config_path,
            serde_json::to_string(&OrgConfig { folder: org_dir.to_string_lossy().into() })?,
        )?;

        let s1 = collect_org(&vault)?;
        assert_eq!(s1.created, 1);

        let s2 = collect_org(&vault)?;
        assert_eq!(s2.created, 0, "re-sync should not re-create");
        assert_eq!(s2.open, 1);

        let _ = fs::remove_dir_all(&org_dir);
        Ok(())
    }

    /// No-op when the config folder is not set.
    #[test]
    fn no_folder_configured_is_noop() -> Result<()> {
        let vault = temp_vault("noop");
        let stats = collect_org(&vault)?;
        assert_eq!(stats.open, 0);
        assert_eq!(stats.created, 0);
        Ok(())
    }

    /// Raw layer is written after a sync.
    #[test]
    fn raw_layer_is_written() -> Result<()> {
        let vault = temp_vault("raw_layer");
        let org_dir = std::env::temp_dir()
            .join(format!("trove-org-mode-orgdir4-{}", std::process::id()));
        let _ = fs::remove_dir_all(&org_dir);
        fs::create_dir_all(&org_dir)?;
        fs::write(org_dir.join("tasks.org"), "* TODO Raw task :tag:\n")?;
        let config_path = vault.resolve(CONFIG_FILE)?;
        if let Some(p) = config_path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(
            &config_path,
            serde_json::to_string(&OrgConfig { folder: org_dir.to_string_lossy().into() })?,
        )?;

        collect_org(&vault)?;

        let raw_dir = vault.root().join("tasks/org-mode/raw");
        assert!(raw_dir.exists(), "raw directory should be created");
        let has_jsonl = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .any(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false));
        assert!(has_jsonl, "raw JSONL should be written");

        let _ = fs::remove_dir_all(&org_dir);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Regression tests for defects fixed in the adversarial-verify pass.

    /// BLOCKING fix: custom #+TODO: keywords (Doom/Spacemacs style) are recognized
    /// as open or done states, not silently dropped.
    #[test]
    fn custom_todo_keywords_parsed() {
        let content = "\
#+TODO: TODO PROG WAIT | DONE KILL
* TODO Normal open task
* PROG In-progress task (custom open kw)
* WAIT Blocked task (custom open kw)
* DONE Standard done task
  CLOSED: [2026-06-16 Tue 10:00]
* KILL Cancelled task (custom done kw)
  CLOSED: [2026-06-16 Tue 10:00]
";
        let tasks = parse_org_file(content, "doom.org", "2026-06-16T10:00:00+00:00");
        assert_eq!(tasks.len(), 5, "all 5 task headings should be recognised; got: {tasks:?}");

        let by_kw: std::collections::HashMap<&str, &OrgTask> =
            tasks.iter().map(|t| (t.keyword.as_str(), t)).collect();

        assert!(!by_kw["PROG"].done, "PROG is an open keyword");
        assert!(!by_kw["WAIT"].done, "WAIT is an open keyword");
        assert!(by_kw["KILL"].done, "KILL is a done keyword");
        assert!(by_kw["DONE"].done, "DONE is a done keyword");
        assert!(!by_kw["TODO"].done, "TODO is an open keyword");
    }

    /// BLOCKING fix: a task previously open and later marked KILL (custom done kw)
    /// must resolve to Completed, not Deleted.
    #[test]
    fn custom_done_keyword_resolves_completed_not_deleted() -> Result<()> {
        let vault = temp_vault("custom_kw_fate");
        let org_dir = std::env::temp_dir()
            .join(format!("trove-org-mode-kw-fate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&org_dir);
        fs::create_dir_all(&org_dir)?;

        // First sync: one open task with custom keywords declared.
        fs::write(
            org_dir.join("tasks.org"),
            "#+TODO: TODO PROG | DONE KILL\n* TODO My Task\n  :PROPERTIES:\n  :ID: kf-1\n  :END:\n",
        )?;
        let config_path = vault.resolve(CONFIG_FILE)?;
        if let Some(p) = config_path.parent() { fs::create_dir_all(p)?; }
        fs::write(
            &config_path,
            serde_json::to_string(&OrgConfig { folder: org_dir.to_string_lossy().into() })?,
        )?;
        let s1 = collect_org(&vault)?;
        assert_eq!(s1.open, 1);
        assert_eq!(s1.created, 1);

        // Second sync: task is now KILL (custom done keyword).
        fs::write(
            org_dir.join("tasks.org"),
            "#+TODO: TODO PROG | DONE KILL\n* KILL My Task\n  CLOSED: [2026-06-16 Tue 10:00]\n  :PROPERTIES:\n  :ID: kf-1\n  :END:\n",
        )?;
        let s2 = collect_org(&vault)?;
        assert_eq!(s2.completed, 1, "KILL should resolve as Completed, not Deleted");
        assert_eq!(s2.deleted, 0, "should NOT be counted as deleted");

        let _ = fs::remove_dir_all(&org_dir);
        Ok(())
    }

    /// MAJOR fix: timed recurring timestamp (SCHEDULED with repeater) preserves
    /// the time component — must not be all-day.
    #[test]
    fn timed_recurring_timestamp_preserves_time() {
        let content = "\
* TODO Weekly standup
  SCHEDULED: <2026-06-20 Sat 12:30 +1w>
";
        let tasks = parse_org_file(content, "recur.org", "2026-06-16T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        // scheduled must be RFC3339 (longer than 10 chars), not a bare date.
        let sched = t.scheduled.as_deref().expect("scheduled should be set");
        assert!(
            sched.len() > 10,
            "timed recurring should not be all-day; got scheduled='{sched}'"
        );
        assert!(
            sched.contains("12:"),
            "time 12:30 should be preserved; got '{sched}'"
        );
        // recurrence cookie must be captured.
        assert_eq!(
            t.recurrence_cookie.as_deref(),
            Some("+1w"),
            "repeater should be captured"
        );
    }

    /// MAJOR fix: recurrence cookie is mapped to RRULE in the contract layer.
    #[test]
    fn recurrence_cookie_to_rrule() {
        // +1w → FREQ=WEEKLY;INTERVAL=1
        assert_eq!(cookie_to_rrule("+1w"), Some("FREQ=WEEKLY;INTERVAL=1".to_string()));
        // .+2d → FREQ=DAILY;INTERVAL=2
        assert_eq!(cookie_to_rrule(".+2d"), Some("FREQ=DAILY;INTERVAL=2".to_string()));
        // ++1m → FREQ=MONTHLY;INTERVAL=1
        assert_eq!(cookie_to_rrule("++1m"), Some("FREQ=MONTHLY;INTERVAL=1".to_string()));
        // +1y → FREQ=YEARLY;INTERVAL=1
        assert_eq!(cookie_to_rrule("+1y"), Some("FREQ=YEARLY;INTERVAL=1".to_string()));
    }

    /// MAJOR fix: recurrence is set in the Task contract row when a cookie is present.
    #[test]
    fn to_task_sets_recurrence_from_cookie() {
        let content = "\
* TODO Pay rent
  SCHEDULED: <2026-06-01 Mon +1m>
";
        let tasks = parse_org_file(content, "bills.org", "2026-06-16T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let task = to_task(&tasks[0]);
        assert!(
            task.recurrence.is_some(),
            "recurrence should be set for tasks with a repeater cookie"
        );
        let rec = task.recurrence.unwrap();
        assert!(
            rec.contains("MONTHLY") || rec.contains("+1m"),
            "recurrence should reference monthly; got '{rec}'"
        );
    }

    /// MAJOR fix: body text maps to the contract 'notes' field, not only extra.
    #[test]
    fn body_text_maps_to_notes() {
        let content = "\
* TODO Read the book
  This is the body text that should appear in notes.
  It has multiple lines.
";
        let tasks = parse_org_file(content, "notes.org", "2026-06-16T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        let task = to_task(&tasks[0]);
        assert!(
            !task.notes.is_empty(),
            "notes should be populated from the body"
        );
        assert!(
            task.notes.contains("body text"),
            "notes should contain the body; got '{}'",
            task.notes
        );
    }

    /// MINOR fix: due = DEADLINE only; SCHEDULED goes into start, not due.
    #[test]
    fn due_is_deadline_only_not_scheduled() {
        // Task with only SCHEDULED (no DEADLINE): due should be None.
        let content_sched_only = "\
* TODO Call dentist
  SCHEDULED: <2026-06-20 Sat>
";
        let tasks = parse_org_file(content_sched_only, "t.org", "2026-06-16T10:00:00+00:00");
        let task = to_task(&tasks[0]);
        assert!(
            task.due.is_none(),
            "due should be None when only SCHEDULED is set; got {:?}",
            task.due
        );
        assert!(task.start.is_some(), "start should be set from SCHEDULED");

        // Task with both SCHEDULED and DEADLINE: due = deadline.
        let content_both = "\
* TODO Sprint review
  SCHEDULED: <2026-06-19 Fri>  DEADLINE: <2026-06-22 Mon>
";
        let tasks2 = parse_org_file(content_both, "t.org", "2026-06-16T10:00:00+00:00");
        let task2 = to_task(&tasks2[0]);
        assert_eq!(
            task2.due.as_deref(),
            Some("2026-06-22"),
            "due should be DEADLINE date"
        );
        assert_eq!(
            task2.start.as_deref(),
            Some("2026-06-19"),
            "start should be SCHEDULED date"
        );
    }

    /// MINOR fix: created field populated from :CREATED: property.
    #[test]
    fn created_from_property() {
        let content = "\
* TODO Write release notes
  :PROPERTIES:
  :ID: rn-1
  :CREATED: 2026-01-15
  :END:
";
        let tasks = parse_org_file(content, "notes.org", "2026-06-16T10:00:00+00:00");
        let task = to_task(&tasks[0]);
        assert_eq!(
            task.created.as_deref(),
            Some("2026-01-15"),
            "created should come from :CREATED: property"
        );
    }

    /// MINOR fix: time-range end is preserved in extra.
    #[test]
    fn time_range_end_preserved_in_extra() {
        // Parse a timestamp with a time range "10:00-12:00".
        let parsed = parse_org_ts_full("<2026-06-20 Sat 10:00-12:00>");
        assert!(parsed.is_some());
        let p = parsed.unwrap();
        assert!(p.value.contains("10:"), "start time should be 10:xx");
        assert_eq!(p.range_end.as_deref(), Some("12:00"), "end time should be captured");

        // Confirm it flows into OrgTask and then to_task extra.
        let content = "\
* TODO Morning block
  SCHEDULED: <2026-06-20 Sat 10:00-12:00>
";
        let tasks = parse_org_file(content, "t.org", "2026-06-16T10:00:00+00:00");
        assert_eq!(tasks.len(), 1);
        // The raw OrgTask should have the range end.
        assert_eq!(
            tasks[0].time_range_end.as_deref(),
            Some("12:00"),
            "time_range_end should be stored on OrgTask"
        );
        // And the contract Task extra should carry it.
        let task = to_task(&tasks[0]);
        assert_eq!(
            task.extra.get("time_range_end").and_then(|v| v.as_str()),
            Some("12:00"),
            "time_range_end should be in Task.extra"
        );
    }

    /// Verify fast-access key stripping in #+TODO: directives (e.g. "TODO(t)").
    #[test]
    fn custom_kw_fast_access_stripped() {
        let content = "\
#+TODO: TODO(t) NEXT(n) WAIT(w) | DONE(d) CANCELLED(c)
* NEXT Prepare slides
* WAIT Manager approval
* CANCELLED Old idea
  CLOSED: [2026-06-16 Tue 09:00]
";
        let tasks = parse_org_file(content, "f.org", "2026-06-16T10:00:00+00:00");
        assert_eq!(tasks.len(), 3, "NEXT, WAIT, CANCELLED all recognised; got: {tasks:?}");
        let cancelled = tasks.iter().find(|t| t.keyword == "CANCELLED").unwrap();
        assert!(cancelled.done, "CANCELLED should be a done state");
        let next = tasks.iter().find(|t| t.keyword == "NEXT").unwrap();
        assert!(!next.done, "NEXT should be an open state");
    }
}
