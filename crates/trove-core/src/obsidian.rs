//! Obsidian — local Markdown vault folder sync.
//!
//! Obsidian stores notes as plain `.md` files in any directory the user
//! chooses (their "vault"). This collector walks that directory, reads every
//! `.md` file, parses YAML frontmatter (the `---` block at file start), and
//! writes both a contract layer and a raw layer into the Trove vault.
//!
//! **No API, no auth.** The user selects the vault directory once; the path
//! is persisted in `.trove/obsidian-sync.json`. The collector then runs
//! periodically, using a per-file mtime map as an incremental watermark so
//! only new or changed notes are re-ingested. The hidden `.obsidian/`
//! subdirectory (workspace config / plugin JSON) is ignored entirely — it is
//! not user content.
//!
//! **Contract layer:** `notes/obsidian/YYYY-MM.jsonl` — one [`crate::notes::Note`]
//! per note, partitioned by the local month of `created`, deduped/upserted by
//! `id` (the vault-relative path, slash-normalised and case-preserved — immutable
//! for the life of the file). Mirrors the Bear collector's snapshot+upsert
//! mechanics.
//!
//! **Raw layer:** `notes/obsidian/raw/YYYY-MM.jsonl` — full fidelity: every
//! frontmatter key verbatim, the full Markdown body, vault-relative path, and
//! file mtime. Partitioned by the same created month so the two layers land
//! in the same file.
//!
//! **Frontmatter parser:** hand-rolled, no external YAML dep. Covers the
//! common Obsidian subset: scalar values, block lists (`key:\n  - item`), and
//! inline YAML lists (`key: [a, b]`). Anything it can't parse is preserved
//! verbatim in `extra.raw_frontmatter`. Confirmed against the official
//! Obsidian Properties docs (tags as lowercase `tags:`, date format
//! `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS`, array forms both block and inline).
//!
//! **Title resolution (priority):**
//!  1. Frontmatter `title:` field.
//!  2. First `# Heading` line in the body.
//!  3. File stem (filename without `.md`).
//!
//! Brief: `docs/integrations/obsidian.md`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// How often the Obsidian vault is rescanned. Hourly matches Bear / Voice
/// Memos — notes change infrequently relative to messages.
pub const OBSIDIAN_SYNC_SECS: u64 = 3600;

const SYNC_FILE: &str = ".trove/obsidian-sync.json";
const SOURCE: &str = "obsidian";
const NOTES_DIR: &str = "notes/obsidian";
const RAW_DIR: &str = "notes/obsidian/raw";

// ---------------------------------------------------------------------------
// DEF hooks

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_obsidian()?;
    Ok(CollectOutcome::note_if(s.new_notes > 0, || {
        format!("imported {} Obsidian note(s)", s.new_notes)
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_obsidian_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "obsidian",
        name: "Obsidian",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Periodically syncs your Obsidian vault by reading the \
                      plain Markdown files in the folder you choose, \
                      preserving YAML frontmatter and folder structure.",
        domain: "notes",
        vault_path: "notes/obsidian/",
        toggleable: true,
        setup: &[
            "Point Trove at your Obsidian vault folder (the top-level directory \
             that contains your .md files). The .obsidian/ config subfolder is \
             always ignored.",
        ],
        caveats: "The .obsidian/ config folder is ignored; only .md note files \
                 are collected. Encrypted vaults (Obsidian Sync encryption) \
                 are not supported — the collector reads plain files only. \
                 YAML frontmatter is parsed for common scalar and list fields; \
                 deeply nested objects land in extra.raw_frontmatter verbatim.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(OBSIDIAN_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Sync state

/// Incremental sync state persisted in `.trove/obsidian-sync.json`.
/// Rebuildable by re-scanning — the watermark is advisory, dedupe is by `id`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ObsidianSyncState {
    /// RFC3339 local time of the last successful sync pass.
    pub updated: String,
    /// Absolute path to the user's chosen Obsidian vault directory.
    /// Empty until the user has selected a vault.
    pub vault_path: String,
    /// Per-file mtime watermark: vault-relative path → Unix seconds of the
    /// file's mtime as of the last successful import. Only files whose mtime
    /// has advanced since are re-read.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mtimes: HashMap<String, u64>,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Serialize)]
pub struct ObsidianSyncStats {
    /// No vault path is configured yet.
    pub no_vault: bool,
    pub new_notes: u64,
}

// ---------------------------------------------------------------------------
// Path helpers

/// Stable vault-relative id from a file path: forward-slash separated,
/// case-preserved, always relative to `vault_root`. Used as the Note `id`
/// and the mtime key. Panics if `path` is not under `vault_root` (the
/// caller guarantees this during the walk). Case is preserved (not
/// lowercased) because APFS is case-sensitive by default and we want the
/// path to remain a stable round-trip identity.
fn vault_rel_path(vault_root: &Path, path: &Path) -> String {
    path.strip_prefix(vault_root)
        .expect("path is always under vault_root during walk")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Unix epoch seconds from a `SystemTime`, or 0 on overflow (pre-epoch files
/// are treated as always-new to force re-import).
fn mtime_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Frontmatter parser
//
// Obsidian YAML frontmatter is delimited by `---` on its own line at the very
// start of the file. We parse the common subset:
//
//   - Scalar:     `key: value`
//   - Block list: `key:\n  - item\n  - item`
//   - Inline list:`key: [a, b, c]`
//
// Output: (frontmatter map, body_after_delimiter). Body is everything after
// the closing `---\n` (or `---` at EOF). If there is no valid frontmatter
// block the whole file is the body and the map is empty.

/// One parsed frontmatter value. We only distinguish scalars from lists so
/// the extractor can route them correctly without a full YAML type system.
#[derive(Debug, Clone)]
enum FmVal {
    Scalar(String),
    List(Vec<String>),
}

/// Parse the leading `---`…`---` block, returning `(map, body)`.
/// `body` is the text after the closing delimiter (empty string when the
/// delimiter is at EOF). If no valid delimiter pair is found, the whole
/// content is the body and the map is empty.
fn parse_frontmatter(content: &str) -> (HashMap<String, FmVal>, &str) {
    // Must start with exactly `---` followed by `\n` (or `\r\n`).
    let after_open = if let Some(rest) = content.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = content.strip_prefix("---\r\n") {
        rest
    } else {
        return (HashMap::new(), content);
    };

    // Find the closing `---` on its own line.
    let close_marker = "\n---";
    let close_pos = match after_open.find(close_marker) {
        Some(p) => p,
        None => return (HashMap::new(), content),
    };

    let fm_block = &after_open[..close_pos];
    // Body starts after `\n---` and the rest of that line (including any
    // trailing spaces before the newline). We skip to and consume the next
    // newline so that a `--- ` delimiter with a trailing space doesn't leave
    // a stray whitespace line at the start of the body.
    let after_close = &after_open[close_pos + close_marker.len()..];
    let body = if let Some(nl) = after_close.find('\n') {
        // Advance past the entire remainder of the delimiter line.
        &after_close[nl + 1..]
    } else {
        // `---` (possibly with trailing chars) at EOF, no following newline.
        ""
    };

    let map = parse_fm_block(fm_block);
    (map, body)
}

/// Parse the interior of the frontmatter block (between the `---` delimiters)
/// into a key→value map. Best-effort: unparseable lines are skipped silently.
fn parse_fm_block(block: &str) -> HashMap<String, FmVal> {
    let mut map: HashMap<String, FmVal> = HashMap::new();
    let lines: Vec<&str> = block.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Skip blank lines.
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        // A top-level key must start at column 0 (no leading spaces).
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation of a previous block list — handled below.
            i += 1;
            continue;
        }
        // `key: value` or `key:` (value is a block list on subsequent lines).
        let colon = match line.find(':') {
            Some(p) => p,
            None => { i += 1; continue; }
        };
        let key = line[..colon].trim().to_string();
        if key.is_empty() {
            i += 1;
            continue;
        }
        let raw_val = line[colon + 1..].trim();

        if raw_val.is_empty() {
            // Block list: following indented `- item` lines.
            i += 1;
            let mut items = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                let trimmed = l.trim();
                if trimmed.starts_with("- ") || trimmed == "-" {
                    let item = trimmed.trim_start_matches('-').trim().to_string();
                    if !item.is_empty() {
                        items.push(item);
                    }
                    i += 1;
                } else if trimmed.is_empty() {
                    // Blank line inside a block list — tolerate, keep reading.
                    i += 1;
                } else {
                    break; // Next top-level key.
                }
            }
            if items.len() == 1 {
                // Single-item block lists serialize as a list, but represent
                // them as such for tags extraction uniformity.
                map.insert(key, FmVal::List(items));
            } else if !items.is_empty() {
                map.insert(key, FmVal::List(items));
            }
            // Empty block list → omit.
        } else if raw_val.starts_with('[') && raw_val.ends_with(']') {
            // Inline YAML list: `[item1, item2, ...]`
            let inner = &raw_val[1..raw_val.len() - 1];
            let items: Vec<String> = inner
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !items.is_empty() {
                map.insert(key, FmVal::List(items));
            }
            i += 1; // advance past the inline-list line
        } else {
            // Scalar: strip surrounding quotes if present.
            let val = raw_val.trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() {
                map.insert(key, FmVal::Scalar(val));
            }
            i += 1;
        }
    }
    map
}

/// Extract a scalar string from the frontmatter map. Coerces a single-item
/// list to its element (some editors emit `title: [My Note]`).
fn fm_scalar(map: &HashMap<String, FmVal>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(FmVal::Scalar(s)) => Some(s.clone()),
        Some(FmVal::List(v)) if v.len() == 1 => Some(v[0].clone()),
        _ => None,
    }
}

/// Extract a string list from the frontmatter map.
fn fm_list(map: &HashMap<String, FmVal>, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(FmVal::List(v)) => v.clone(),
        Some(FmVal::Scalar(s)) => {
            // A single tag written as a scalar (not a list).
            if s.is_empty() { vec![] } else { vec![s.clone()] }
        }
        None => vec![],
    }
}

/// Serialize a frontmatter map to a JSON object for the raw layer. Scalars →
/// strings, lists → arrays. Keys exactly as in the source file.
fn fm_to_json(map: &HashMap<String, FmVal>) -> Map<String, Value> {
    let mut obj = Map::new();
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort(); // deterministic order for test fixtures
    for k in keys {
        let v = match &map[k] {
            FmVal::Scalar(s) => Value::String(s.clone()),
            FmVal::List(items) => {
                Value::Array(items.iter().cloned().map(Value::String).collect())
            }
        };
        obj.insert(k.to_string(), v);
    }
    obj
}

// ---------------------------------------------------------------------------
// Title extraction

/// Resolve the note title (priority: frontmatter `title:` → first `# ` →
/// file stem). Never returns an empty string.
fn resolve_title(fm: &HashMap<String, FmVal>, body: &str, path: &Path) -> String {
    if let Some(t) = fm_scalar(fm, "title") {
        if !t.is_empty() {
            return t;
        }
    }
    // First `# Heading` in the body.
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            let h = heading.trim().to_string();
            if !h.is_empty() {
                return h;
            }
        }
    }
    // File stem.
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Date helpers

/// Convert an Obsidian date/datetime string to RFC3339 local time.
///
/// Accepts the full range of formats Obsidian and its popular plugins emit:
/// - RFC3339 / ISO-8601 with timezone suffix: `2026-06-10T14:30:00-07:00`,
///   `2026-06-10T14:30:00Z` (handled by `parse_from_rfc3339`).
/// - Naive datetime with `T` separator: `2026-06-10T14:30:00` (bare, no tz).
/// - Naive datetime with space separator: `2026-06-10 14:30:00` (Templater
///   `tp.file.creation_date("YYYY-MM-DD HH:mm:ss")` default).
/// - Naive datetime, seconds omitted: `2026-06-10T14:30` or `2026-06-10 14:30`.
/// - Naive datetime with fractional seconds: `2026-06-10T14:30:00.123` or with
///   space: `2026-06-10 14:30:00.123`.
/// - Date only: `2026-06-10` (treated as local midnight).
///
/// Offset-aware inputs are converted to the local timezone. Naive inputs are
/// interpreted as local time. Returns `None` only for strings that match none
/// of the above patterns.
fn parse_obsidian_date(s: &str) -> Option<String> {
    let s = s.trim();

    // 1. RFC3339 / ISO-8601 with explicit timezone offset or Z.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }

    // 2. Naive formats: try from most specific to least.
    //    Use both T-separator and space-separator variants.
    let naive_fmts: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S%.f", // with fractional seconds, T-sep
        "%Y-%m-%d %H:%M:%S%.f", // with fractional seconds, space-sep
        "%Y-%m-%dT%H:%M:%S",    // no fractions, T-sep
        "%Y-%m-%d %H:%M:%S",    // no fractions, space-sep (Templater default)
        "%Y-%m-%dT%H:%M",       // seconds omitted, T-sep
        "%Y-%m-%d %H:%M",       // seconds omitted, space-sep
    ];
    for fmt in naive_fmts {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Local.from_local_datetime(&dt).earliest()?.to_rfc3339());
        }
    }

    // 3. Date only → local midnight.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0)?;
        return Some(Local.from_local_datetime(&dt).earliest()?.to_rfc3339());
    }

    None
}

use chrono::TimeZone as _;

/// Unix epoch seconds → RFC3339 local time.
fn unix_to_local_rfc3339(secs: u64) -> String {
    let dt = DateTime::<Local>::from(
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
    );
    dt.to_rfc3339()
}

// ---------------------------------------------------------------------------
// Core import

/// One note file parsed and ready to write.
struct ParsedNote {
    /// Vault-relative path (the stable id).
    rel_path: String,
    contract: Note,
    raw: Value,
}

/// Parse a single `.md` file under `vault_root`. Returns `None` on I/O error
/// (file vanished between the walk and the read — benign).
fn parse_md_file(
    vault_root: &Path,
    path: &Path,
    mtime_s: u64,
) -> Option<ParsedNote> {
    let content = fs::read_to_string(path).ok()?;
    let rel = vault_rel_path(vault_root, path);
    let (fm, body) = parse_frontmatter(&content);

    // Dates: prefer frontmatter fields (user-authored), fall back to mtime.
    // Common frontmatter date fields: created/date_created/creation date,
    // modified/date_modified/updated.
    let fm_created = fm_scalar(&fm, "created")
        .or_else(|| fm_scalar(&fm, "date_created"))
        .or_else(|| fm_scalar(&fm, "date"))
        .and_then(|s| parse_obsidian_date(&s));
    let fm_modified = fm_scalar(&fm, "modified")
        .or_else(|| fm_scalar(&fm, "date_modified"))
        .or_else(|| fm_scalar(&fm, "updated"))
        .and_then(|s| parse_obsidian_date(&s));

    let mtime_rfc = unix_to_local_rfc3339(mtime_s);
    let created = fm_created.unwrap_or_else(|| mtime_rfc.clone());
    let modified = fm_modified.unwrap_or_else(|| mtime_rfc.clone());

    // Title.
    let title = resolve_title(&fm, body, path);

    // Tags from `tags:` frontmatter field.
    // Strip a single leading `#` if present (Obsidian Properties UI does this
    // automatically; hand-edited files sometimes retain it — e.g. `- #garden`).
    // The raw/extra.frontmatter layer preserves the verbatim value.
    let tags: Vec<String> = fm_list(&fm, "tags")
        .into_iter()
        .map(|t| t.strip_prefix('#').map(|s| s.to_string()).unwrap_or(t))
        .collect();

    // Folder: parent directory relative to vault root (empty for root-level
    // notes).
    let folder: String = {
        let parent = Path::new(&rel)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        parent
    };

    // Contract: Note struct.
    let mut note = Note::new(SOURCE, &rel);
    note.title = title.clone();
    note.body = body.to_string();
    note.created = created.clone();
    note.modified = modified.clone();
    note.tags = tags.clone();
    note.folder = folder.clone();

    // Extra: frontmatter keys that aren't already mapped to contract fields,
    // plus `aliases` and any unknown keys.
    let contract_keys: HashSet<&str> =
        ["title", "created", "date_created", "date", "modified", "date_modified", "updated", "tags"]
            .into();
    let mut extra = Map::new();
    let fm_json = fm_to_json(&fm);
    for (k, v) in &fm_json {
        if !contract_keys.contains(k.as_str()) {
            extra.insert(k.clone(), v.clone());
        }
    }
    // Always record the full frontmatter verbatim for auditability.
    if !fm_json.is_empty() {
        extra.insert("frontmatter".into(), Value::Object(fm_json.clone()));
    }
    note.extra = extra;

    // Raw layer: full fidelity.
    let mut raw_obj = Map::new();
    raw_obj.insert("source".into(), Value::String(SOURCE.to_string()));
    raw_obj.insert("id".into(), Value::String(rel.clone()));
    raw_obj.insert("path".into(), Value::String(rel.clone()));
    raw_obj.insert("title".into(), Value::String(title));
    raw_obj.insert("body".into(), Value::String(body.to_string()));
    raw_obj.insert("created".into(), Value::String(created.clone()));
    raw_obj.insert("modified".into(), Value::String(modified.clone()));
    if !tags.is_empty() {
        raw_obj.insert(
            "tags".into(),
            Value::Array(tags.into_iter().map(Value::String).collect()),
        );
    }
    if !folder.is_empty() {
        raw_obj.insert("folder".into(), Value::String(folder));
    }
    raw_obj.insert("file_mtime_secs".into(), Value::from(mtime_s));
    // Full frontmatter as sub-object.
    if !fm_json.is_empty() {
        raw_obj.insert("frontmatter".into(), Value::Object(fm_json));
    }
    // Immutable partition key (created month) — required by upsert_raw_by_month.
    raw_obj.insert("_created".into(), Value::String(created.clone()));

    Some(ParsedNote {
        rel_path: rel,
        contract: note,
        raw: Value::Object(raw_obj),
    })
}

// ---------------------------------------------------------------------------
// Filesystem walk

/// Walk `dir` recursively, collecting every `.md` file path and its mtime.
/// Ignores `.obsidian/` (Obsidian's config/plugin folder) and any hidden
/// directories (name starts with `.`). The vault root itself may live anywhere
/// so no path assumptions are made beyond "it's a readable directory".
///
/// Uses lstat (via `fs::symlink_metadata`) for consistency with cloud-mirror
/// safety — though Obsidian vaults are local, some users sync via cloud
/// providers that may introduce SF_DATALESS placeholders. We never open
/// anything other than confirmed regular files.
fn walk_vault(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden dirs (including .obsidian/).
        if name_str.starts_with('.') {
            continue;
        }

        // Use symlink_metadata so we don't follow symlinks into unexpected
        // places (cloud-mirror placeholder guard from cloud_folder.rs).
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            walk_vault(&path, out);
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "md") {
            let mtime = meta.modified().map(mtime_secs).unwrap_or(0);
            out.push((path, mtime));
        }
    }
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One incremental sync pass over the user's Obsidian vault.
    ///
    /// - If `vault_path` in the sync state is empty (user hasn't chosen a
    ///   vault yet), returns `no_vault: true` and does nothing.
    /// - Otherwise, walks the vault, re-imports any `.md` files whose mtime
    ///   advanced since last run, and updates the state.
    pub fn collect_obsidian(&self) -> Result<ObsidianSyncStats> {
        let mut state = self.read_obsidian_sync().unwrap_or_default();
        if state.vault_path.is_empty() {
            return Ok(ObsidianSyncStats { no_vault: true, new_notes: 0 });
        }
        let vault_root = PathBuf::from(&state.vault_path);
        if !vault_root.is_dir() {
            // Directory removed or not mounted — skip quietly.
            return Ok(ObsidianSyncStats { no_vault: true, new_notes: 0 });
        }

        // Collect all .md files and their mtimes.
        let mut files: Vec<(PathBuf, u64)> = Vec::new();
        walk_vault(&vault_root, &mut files);

        // Filter to only changed/new files using the mtime watermark.
        let changed: Vec<(PathBuf, u64)> = files
            .iter()
            .filter(|(path, mtime)| {
                let rel = vault_rel_path(&vault_root, path);
                state.mtimes.get(&rel).copied().unwrap_or(0) < *mtime
            })
            .cloned()
            .collect();

        if changed.is_empty() {
            state.updated = Local::now().to_rfc3339();
            self.write_obsidian_sync(&state)?;
            return Ok(ObsidianSyncStats { no_vault: false, new_notes: 0 });
        }

        // Parse all changed files.
        let mut contract_notes: Vec<Note> = Vec::new();
        let mut raw_rows: Vec<Value> = Vec::new();
        for (path, mtime) in &changed {
            if let Some(parsed) = parse_md_file(&vault_root, path, *mtime) {
                state.mtimes.insert(parsed.rel_path.clone(), *mtime);
                contract_notes.push(parsed.contract);
                raw_rows.push(parsed.raw);
            }
        }

        let n = contract_notes.len() as u64;
        if !contract_notes.is_empty() {
            self.upsert_obsidian_notes(&contract_notes)?;
            self.upsert_obsidian_raw(&raw_rows)?;
        }

        state.updated = Local::now().to_rfc3339();
        self.write_obsidian_sync(&state)?;
        Ok(ObsidianSyncStats { no_vault: false, new_notes: n })
    }

    /// Upsert contract notes into `notes/obsidian/YYYY-MM.jsonl`, partitioned
    /// by the month of `created`, deduped by `id`. Mirrors `upsert_bear_notes`.
    pub fn upsert_obsidian_notes(&self, notes: &[Note]) -> Result<()> {
        use std::collections::HashMap as HM;
        let mut by_month: HM<String, Vec<&Note>> = HM::new();
        for n in notes {
            if let Some(key) = Partition::Month.key(&n.created) {
                by_month.entry(key.to_string()).or_default().push(n);
            }
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|n| n.id.as_str()).collect();
            let mut merged: Vec<Note> = stream
                .read::<Note>(&month)?
                .into_iter()
                .filter(|existing| !incoming_ids.contains(existing.id.as_str()))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{NOTES_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Upsert raw rows into `notes/obsidian/raw/YYYY-MM.jsonl`, partitioned by
    /// the month of `_created`, deduped by `id`.
    pub fn upsert_obsidian_raw(&self, rows: &[Value]) -> Result<()> {
        use std::collections::HashMap as HM;
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HM<String, Vec<&Value>> = HM::new();
        for v in rows {
            if let Some(key) = Partition::Month.key(month_of(v)) {
                by_month.entry(key.to_string()).or_default().push(v);
            }
        }
        let stream = self.stream(RAW_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|v| id_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|existing| !incoming_ids.contains(id_of(existing)))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{RAW_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Read the persisted sync state, if any.
    pub fn read_obsidian_sync(&self) -> Option<ObsidianSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_obsidian_sync(&self, state: &ObsidianSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)
            .with_context(|| format!("writing {SYNC_FILE}"))?;
        fs::rename(&tmp, &path).with_context(|| format!("publishing {SYNC_FILE}"))?;
        Ok(())
    }

    /// Set the Obsidian vault path (called from the UI setup flow). Persists
    /// the path in the sync state so the next collect pass can find the vault.
    /// A previous path is replaced; the mtime watermark is reset so all notes
    /// are re-imported from the new vault.
    pub fn set_obsidian_vault_path(&self, path: &str) -> Result<()> {
        let state = ObsidianSyncState {
            vault_path: path.to_string(),
            updated: String::new(),
            mtimes: HashMap::new(),
        };
        self.write_obsidian_sync(&state)
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh Vault in a unique temp directory.
    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-obsidian-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a synthetic Obsidian vault directory with the given files.
    /// `files`: `[(relative_path, content)]`. Returns the vault root.
    fn fake_vault(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("trove-obsidian-src-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (rel, content) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        root
    }

    // -----------------------------------------------------------------------
    // Frontmatter parser unit tests

    #[test]
    fn parses_scalar_frontmatter() {
        let content = "---\ntitle: My Note\ncreated: 2026-06-10\n---\n\nBody text here.\n";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm_scalar(&fm, "title").as_deref(), Some("My Note"));
        assert_eq!(fm_scalar(&fm, "created").as_deref(), Some("2026-06-10"));
        assert_eq!(body.trim(), "Body text here.");
    }

    #[test]
    fn parses_block_list_tags() {
        let content = "---\ntags:\n  - garden\n  - spring\n---\n# My Note\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(fm_list(&fm, "tags"), vec!["garden", "spring"]);
    }

    #[test]
    fn parses_inline_list_tags() {
        let content = "---\ntags: [garden, spring, reading]\n---\nBody.\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(fm_list(&fm, "tags"), vec!["garden", "spring", "reading"]);
    }

    #[test]
    fn no_frontmatter_returns_empty_map() {
        let content = "# Just a heading\n\nNo frontmatter here.\n";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty(), "no frontmatter → empty map");
        assert!(body.contains("Just a heading"));
    }

    #[test]
    fn frontmatter_without_closing_delimiter_is_rejected() {
        let content = "---\ntitle: Incomplete\n\nNo closing delimiter.";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty(), "unterminated → empty map");
        assert!(body.contains("Incomplete"), "whole content is body: {body}");
    }

    #[test]
    fn title_from_frontmatter_wins_over_heading() {
        let fm = {
            let mut m = HashMap::new();
            m.insert("title".to_string(), FmVal::Scalar("FM Title".to_string()));
            m
        };
        let body = "# Heading Title\n\nBody.";
        let path = Path::new("note.md");
        assert_eq!(resolve_title(&fm, body, path), "FM Title");
    }

    #[test]
    fn title_falls_back_to_heading() {
        let fm = HashMap::new();
        let body = "# Heading Title\n\nBody.";
        let path = Path::new("note.md");
        assert_eq!(resolve_title(&fm, body, path), "Heading Title");
    }

    #[test]
    fn title_falls_back_to_filename_stem() {
        let fm = HashMap::new();
        let body = "No heading here.";
        let path = Path::new("my-great-note.md");
        assert_eq!(resolve_title(&fm, body, path), "my-great-note");
    }

    #[test]
    fn date_parsing_handles_date_and_datetime() {
        // Date only → local midnight.
        let d = parse_obsidian_date("2026-06-10").unwrap();
        assert!(d.starts_with("2026-06-10"), "got: {d}");
        // Datetime bare (T-sep, no tz).
        let dt = parse_obsidian_date("2026-06-10T14:30:00").unwrap();
        assert!(dt.starts_with("2026-06-10"), "got: {dt}");
        // Junk → None.
        assert!(parse_obsidian_date("not a date").is_none());
    }

    #[test]
    fn date_parsing_tz_offset_uses_authored_date() {
        // RFC3339 with negative offset — month must be January, not the local
        // machine month. This is the core correctness regression from the defect.
        let d = parse_obsidian_date("2026-01-05T09:00:00-08:00").unwrap();
        assert!(d.starts_with("2026-01-05"), "tz-offset date: got {d}");

        // RFC3339 with Z (UTC).
        let z = parse_obsidian_date("2026-01-05T09:00:00Z").unwrap();
        assert!(z.starts_with("2026-01-05"), "Z-suffix date: got {z}");
    }

    #[test]
    fn date_parsing_space_separator_templater_style() {
        // Templater default: tp.file.creation_date("YYYY-MM-DD HH:mm:ss")
        let d = parse_obsidian_date("2026-01-05 09:00:00").unwrap();
        assert!(d.starts_with("2026-01-05"), "space-sep date: got {d}");

        // Space-sep without seconds.
        let d2 = parse_obsidian_date("2026-01-05 09:00").unwrap();
        assert!(d2.starts_with("2026-01-05"), "space-sep no-secs: got {d2}");
    }

    #[test]
    fn date_parsing_fractional_seconds() {
        // Some plugins emit milliseconds.
        let d = parse_obsidian_date("2026-01-05T09:00:00.123").unwrap();
        assert!(d.starts_with("2026-01-05"), "fractional secs: got {d}");
    }

    #[test]
    fn date_parsing_no_seconds() {
        // Some plugins omit seconds: T14:30
        let d = parse_obsidian_date("2026-01-05T09:00").unwrap();
        assert!(d.starts_with("2026-01-05"), "no-secs T-sep: got {d}");
    }

    #[test]
    fn tz_note_partitions_to_authored_month() {
        // A note with a tz-bearing frontmatter date in January must land in the
        // 2026-01 partition, NOT the current month's partition.
        let v = temp_vault("tzpart");
        let src = fake_vault("tzpart", &[
            ("journal.md",
             "---\ncreated: 2026-01-05T09:00:00-08:00\nmodified: 2026-01-05T09:00:00-08:00\ntitle: January entry\n---\nBody text.\n"),
        ]);
        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        let stats = v.collect_obsidian().unwrap();
        assert_eq!(stats.new_notes, 1);

        let jan = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-01").unwrap();
        assert_eq!(jan.len(), 1, "note must be in 2026-01 partition, not current month");
        assert!(jan[0].created.starts_with("2026-01-05"), "created: {}", jan[0].created);

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn space_sep_note_partitions_to_authored_month() {
        // A note with Templater-style space-separated date in February must
        // land in the 2026-02 partition.
        let v = temp_vault("spacepart");
        let src = fake_vault("spacepart", &[
            ("note.md",
             "---\ncreated: 2026-02-14 08:30:00\ntitle: February note\n---\nBody.\n"),
        ]);
        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        let stats = v.collect_obsidian().unwrap();
        assert_eq!(stats.new_notes, 1);

        let feb = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-02").unwrap();
        assert_eq!(feb.len(), 1, "note must be in 2026-02 partition");
        assert!(feb[0].created.starts_with("2026-02-14"), "created: {}", feb[0].created);

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn frontmatter_close_delimiter_with_trailing_space() {
        // `--- ` (trailing space) must still be recognised as a close delimiter
        // and must NOT leave a stray whitespace line at the start of the body.
        let content = "---\ntitle: Test\n--- \nBody text.\n";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm_scalar(&fm, "title").as_deref(), Some("Test"),
            "frontmatter parsed correctly");
        assert!(!body.starts_with(' ') && !body.starts_with('\n'),
            "body must not start with stray space/newline: {:?}", body);
        assert!(body.contains("Body text"), "body content intact: {body}");
    }

    #[test]
    fn hash_prefixed_tags_are_stripped() {
        // `- #garden` in frontmatter should store as `garden`, not `#garden`.
        let content = "---\ntags:\n  - #garden\n  - spring\n---\n# My Note\n";
        let (fm, _body) = parse_frontmatter(content);
        // raw fm_list gives verbatim values
        let raw_tags = fm_list(&fm, "tags");
        assert_eq!(raw_tags, vec!["#garden", "spring"]);

        // parse_md_file strips the leading # via the tags normalization path;
        // test via a full vault round-trip.
        let v = temp_vault("hashtag");
        let src = fake_vault("hashtag", &[
            ("note.md", "---\ntags:\n  - #garden\n  - spring\n---\nBody.\n"),
        ]);
        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        v.collect_obsidian().unwrap();

        let notes: Vec<Note> = v
            .stream(NOTES_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|p| v.stream(NOTES_DIR, Partition::Month).read::<Note>(&p).unwrap())
            .collect();
        assert_eq!(notes[0].tags, vec!["garden", "spring"],
            "leading # stripped from tags: {:?}", notes[0].tags);

        let _ = fs::remove_dir_all(src);
    }

    // -----------------------------------------------------------------------
    // Integration tests: import into vault

    #[test]
    fn imports_note_with_frontmatter_tags_and_folder() {
        let v = temp_vault("basic");
        let src = fake_vault("basic", &[
            ("Projects/garden-plan.md",
            "---\ntitle: Garden planting plan\ntags:\n  - garden\n  - spring\ncreated: 2026-03-14\nmodified: 2026-04-02\n---\n\n## Notes\n\nTomatoes in the south bed.\n"),
        ]);

        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        let stats = v.collect_obsidian().unwrap();
        assert!(!stats.no_vault);
        assert_eq!(stats.new_notes, 1);

        // Contract layer.
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar.len(), 1);
        let n = &mar[0];
        assert_eq!(n.source, "obsidian");
        assert_eq!(n.id, "Projects/garden-plan.md");
        assert_eq!(n.title, "Garden planting plan");
        assert!(n.body.contains("Tomatoes"), "body: {}", n.body);
        assert_eq!(n.tags, vec!["garden", "spring"]);
        assert_eq!(n.folder, "Projects");
        assert!(n.created.starts_with("2026-03-14"));
        assert!(n.modified.starts_with("2026-04-02"));
        // extra.frontmatter preserved.
        assert!(n.extra.contains_key("frontmatter"), "frontmatter in extra");

        // Raw layer.
        let raw_mar = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-03").unwrap();
        assert_eq!(raw_mar.len(), 1);
        assert_eq!(raw_mar[0]["id"], "Projects/garden-plan.md");
        assert!(raw_mar[0]["body"].as_str().unwrap().contains("Tomatoes"));
        assert_eq!(raw_mar[0]["folder"], "Projects");

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn imports_note_without_frontmatter_falls_back_to_mtime() {
        let v = temp_vault("nofm");
        let src = fake_vault("nofm", &[
            ("quick-thought.md", "# Quick thought\n\nSomething I noticed.\n"),
        ]);

        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        let stats = v.collect_obsidian().unwrap();
        assert_eq!(stats.new_notes, 1);

        // The note should have a title from the heading and a created from mtime.
        let notes: Vec<Note> = v
            .stream(NOTES_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|p| v.stream(NOTES_DIR, Partition::Month).read::<Note>(&p).unwrap())
            .collect();
        assert_eq!(notes.len(), 1);
        let n = &notes[0];
        assert_eq!(n.title, "Quick thought");
        assert_eq!(n.id, "quick-thought.md");
        assert!(!n.created.is_empty(), "created must have a fallback mtime");

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn skips_obsidian_hidden_config_dir() {
        let v = temp_vault("skipdot");
        let src = fake_vault("skipdot", &[
            ("real-note.md", "# Real note\nContent.\n"),
            (".obsidian/workspace.json", r#"{"key":"value"}"#),
            (".hidden-folder/secret.md", "secret"),
        ]);

        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        let stats = v.collect_obsidian().unwrap();
        // Only real-note.md should be imported; .obsidian/ and .hidden-folder/ skipped.
        assert_eq!(stats.new_notes, 1, "only the visible .md file imported");

        let notes: Vec<Note> = v
            .stream(NOTES_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|p| v.stream(NOTES_DIR, Partition::Month).read::<Note>(&p).unwrap())
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, "real-note.md");

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn incremental_mtime_watermark_skips_unchanged_files() {
        let v = temp_vault("incr");
        let src = fake_vault("incr", &[
            ("note-a.md", "# Note A\nFirst.\n"),
        ]);

        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        let s1 = v.collect_obsidian().unwrap();
        assert_eq!(s1.new_notes, 1);

        // Second pass without changing the file → no new notes.
        let s2 = v.collect_obsidian().unwrap();
        assert_eq!(s2.new_notes, 0, "unchanged file skipped by mtime watermark");

        // Add a new file. note-b.md is not in the watermark at all (key
        // absent) → the mtime check treats 0 < actual_mtime → always new.
        fs::write(src.join("note-b.md"), "# Note B\nSecond.\n").unwrap();

        let s3 = v.collect_obsidian().unwrap();
        // note-b.md is new; note-a.md is unchanged.
        assert_eq!(s3.new_notes, 1, "only the new file picked up");

        let all_notes: Vec<Note> = v
            .stream(NOTES_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|p| v.stream(NOTES_DIR, Partition::Month).read::<Note>(&p).unwrap())
            .collect();
        assert_eq!(all_notes.len(), 2, "both notes in vault after incremental run");

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn upsert_replaces_existing_note_on_edit() {
        let v = temp_vault("upsert");
        let src = fake_vault("upsert", &[
            ("note.md", "# Note\nOriginal body.\n"),
        ]);

        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        v.collect_obsidian().unwrap();

        // Edit the file.
        fs::write(src.join("note.md"), "# Note\nEdited body.\n").unwrap();

        // Reset the watermark for this file to 0 so the collector treats it as
        // new, regardless of filesystem mtime resolution (macOS APFS has
        // 1-second resolution; a sub-second rewrite would otherwise be missed).
        let mut state = v.read_obsidian_sync().unwrap();
        state.mtimes.insert("note.md".to_string(), 0);
        v.write_obsidian_sync(&state).unwrap();

        let s2 = v.collect_obsidian().unwrap();
        assert_eq!(s2.new_notes, 1);

        let all_notes: Vec<Note> = v
            .stream(NOTES_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|p| v.stream(NOTES_DIR, Partition::Month).read::<Note>(&p).unwrap())
            .collect();
        assert_eq!(all_notes.len(), 1, "upsert: exactly one row, not two");
        assert!(all_notes[0].body.contains("Edited"), "body replaced: {}", all_notes[0].body);

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn no_vault_path_is_graceful_no_op() {
        let v = temp_vault("novault");
        // Don't set a vault path → should return no_vault: true.
        let stats = v.collect_obsidian().unwrap();
        assert!(stats.no_vault, "no vault path → graceful no-op");
        assert_eq!(stats.new_notes, 0);
    }

    #[test]
    fn sync_state_persists_and_round_trips() {
        let v = temp_vault("persist");
        let mut state = ObsidianSyncState {
            vault_path: "/Users/alice/Documents/MyVault".to_string(),
            updated: "2026-06-14T10:00:00-07:00".to_string(),
            mtimes: HashMap::new(),
        };
        state.mtimes.insert("note.md".to_string(), 1_718_000_000);
        v.write_obsidian_sync(&state).unwrap();

        let got = v.read_obsidian_sync().unwrap();
        assert_eq!(got.vault_path, "/Users/alice/Documents/MyVault");
        assert_eq!(got.mtimes.get("note.md").copied(), Some(1_718_000_000));
    }

    #[test]
    fn inline_list_tags_from_vault() {
        let v = temp_vault("inline");
        let src = fake_vault("inline", &[
            ("ideas.md", "---\ntags: [ideas, brainstorm, future]\n---\n# Ideas\nContent.\n"),
        ]);

        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        let stats = v.collect_obsidian().unwrap();
        assert_eq!(stats.new_notes, 1);

        let notes: Vec<Note> = v
            .stream(NOTES_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|p| v.stream(NOTES_DIR, Partition::Month).read::<Note>(&p).unwrap())
            .collect();
        assert_eq!(notes[0].tags, vec!["ideas", "brainstorm", "future"]);

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn extra_frontmatter_keys_land_in_extra() {
        let v = temp_vault("extra");
        let src = fake_vault("extra", &[
            ("book.md", "---\ntitle: Book Review\nauthor: Jane Smith\nrating: 5\nfinished: true\n---\nGreat book.\n"),
        ]);

        v.set_obsidian_vault_path(src.to_str().unwrap()).unwrap();
        v.collect_obsidian().unwrap();

        let notes: Vec<Note> = v
            .stream(NOTES_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|p| v.stream(NOTES_DIR, Partition::Month).read::<Note>(&p).unwrap())
            .collect();
        let n = &notes[0];
        // author and rating are not contract fields → land in extra.frontmatter.
        let fm = n.extra.get("frontmatter").and_then(|v| v.as_object()).unwrap();
        assert_eq!(fm.get("author").and_then(|v| v.as_str()), Some("Jane Smith"));
        assert_eq!(fm.get("rating").and_then(|v| v.as_str()), Some("5"));

        let _ = fs::remove_dir_all(src);
    }

    #[test]
    fn serde_back_compat_old_note_lines_still_deserialize() {
        // A Note line written by an older/leaner writer (only source+id) must
        // still deserialize — additive serde evolution.
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-06.jsonl")),
            "{\"source\":\"obsidian\",\"id\":\"old-note.md\"}\n{\"source\":\"obsidian\",\"id\":\"another.md\",\"title\":\"Old\",\"created\":\"2026-06-01T00:00:00-07:00\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "old-note.md");
        assert_eq!(rows[1].title, "Old");
    }
}
