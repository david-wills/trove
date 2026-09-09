//! Snipd — podcast highlight and note app.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/snipd.md.
//!
//! A **Periodic** local-file collector (folder-watch, no network, no auth):
//! the user configures Snipd's Obsidian export plugin (or direct markdown
//! export) to write `.md` files into a folder Trove can read; this module
//! walks that folder and ingests new/changed files.
//!
//! ## Snipd export formats
//!
//! Snipd supports two export paths:
//!
//! 1. **Obsidian sync plugin** (`snipd-app/snipd-obsidian` on GitHub) — uses
//!    the Snipd REST API with a Bearer token to write one `.md` file per
//!    *episode* into `Snipd/Data/{show}/{episode}.md` inside the user's
//!    Obsidian vault. Files contain all snips for the episode embedded via a
//!    configurable template. YAML frontmatter includes at minimum `snips_count`.
//!
//! 2. **Direct markdown export** (Snipd → Profile → Export snips → Markdown) —
//!    produces a ZIP containing one `.md` file per *snip* with YAML frontmatter
//!    keys documented in the research notes: `podcast`, `episode`, `timestamp`,
//!    `tags`, `summary`, and a transcript body section.
//!
//! Both formats land in the same watched folder; both are ingested to the raw
//! layer verbatim. The per-snip contract layer (reading.Highlight) requires a
//! real sample to confirm the frontmatter field names before the parser can be
//! finalized — the raw layer and scaffold are unconditionally built.
//!
//! **Parser status:** PARKED pending a real export sample. The format is
//! plugin/template-driven; field names differ between the Obsidian plugin
//! (per-episode, template-based) and the direct markdown export (per-snip,
//! fixed frontmatter). The raw layer and DEF are fully operational; the
//! highlight mapping skeleton is in place and will be activated once a sample
//! file is available for verification. Set `Needs-sample` + `Needs-David`.
//!
//! ## Two layers
//!
//! - **Raw** — full-fidelity copy of every `.md` file under
//!   `reading/snipd/raw/` (one JSONL file per source-file month, partitioned
//!   by the `created`/`date`/mtime month). The raw object contains the
//!   vault-relative path, all frontmatter fields verbatim, the full body, and
//!   the file mtime. The raw layer is unconditional — it captures whatever the
//!   user's Snipd version writes, regardless of frontmatter shape.
//!
//! - **Contract** (PARKED) — once a real sample is verified, each snip file
//!   maps to a [`crate::reading::Highlight`] under
//!   `reading/snipd/highlights/YYYY-MM.jsonl`: `guid` = stable file id or
//!   frontmatter `snip_id`; `ts` = frontmatter `date`/`created`/file mtime;
//!   `title` = episode; `author` = podcast; `text` = transcript excerpt;
//!   `note` = user note; `tags[]`; `location` = in-episode timestamp; `extra`
//!   = AI summary, episode URL, full transcript.
//!
//! ## Incremental sync
//!
//! Cursor in `.trove/snipd-sync.json` (non-secret, rebuildable): maps each
//! file's vault-relative path → last-seen mtime (Unix seconds). Only files
//! whose mtime has advanced since the last pass are re-read. `guid` dedupe
//! ensures re-ingesting the same file is always idempotent.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// How often the Snipd folder is rescanned. Periodic hourly matches the
/// Obsidian plugin's own sync cadence.
pub const SNIPD_SYNC_SECS: u64 = 3600;

const SYNC_FILE: &str = ".trove/snipd-sync.json";
const SOURCE: &str = "snipd";
const RAW_DIR: &str = "reading/snipd/raw";
// Contract highlights dir — will be written once the per-snip parser is
// un-parked (needs a real export sample to confirm field names).
// const HIGHLIGHTS_DIR: &str = "reading/snipd/highlights";

// ---------------------------------------------------------------------------
// Registry hooks

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_snipd()?;
    if s.no_folder {
        return Ok(CollectOutcome::note("snipd: export folder not configured — see setup"));
    }
    Ok(CollectOutcome::note_if(s.new_files > 0, || {
        format!("snipd: ingested {} new/changed export file(s)", s.new_files)
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_snipd()?;
    let headline = if s.no_folder {
        "Snipd export folder not configured — paste the folder path in the setup step".into()
    } else if s.new_files == 0 {
        "Snipd is up to date — no new export files".into()
    } else {
        format!("Snipd synced — {} new/changed export file(s)", s.new_files)
    };
    let counts = std::collections::BTreeMap::from([("files", s.new_files)]);
    Ok(PullOutcome { headline, counts })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "snipd",
        name: "Snipd",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Collect your podcast highlights and notes from Snipd by watching the \
                      folder that Snipd's Obsidian export plugin (or direct Markdown export) \
                      writes to. Each export file is stored in full fidelity. No login or \
                      network access required — Trove reads only what Snipd has already written.",
        domain: "reading",
        vault_path: "reading/snipd/",
        toggleable: true,
        setup: &[
            "In Snipd: Profile → Export snips → Obsidian (or Markdown) and configure the \
             export folder, or enable the Snipd Obsidian plugin and note where it writes files.",
            "Paste the full path to that folder in the Folder field on this card.",
            "Snipd will write one Markdown file per snip (or episode, for the Obsidian plugin) \
             into that folder; Trove will pick it up on the next sync.",
        ],
        caveats: "Requires the Snipd Obsidian plugin or a manual export to populate the watched \
                  folder. Full episode transcripts require a Snipd Premium subscription. The \
                  per-snip contract parser is in preview — raw files are always stored in full \
                  fidelity. If the Obsidian plugin is used, files are per-episode (all snips \
                  in one file); if the direct export is used, each snip is its own file.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SNIPD_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Sync state

/// Incremental sync state persisted in `.trove/snipd-sync.json`.
/// Rebuildable by re-scanning — dedupe is by file path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnipdSyncState {
    /// RFC3339 local time of the last successful sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Absolute path to the user's configured Snipd export folder.
    /// Empty until the user has set a folder.
    #[serde(default)]
    pub folder_path: String,
    /// Per-file mtime watermark: vault-relative path → Unix seconds.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mtimes: HashMap<String, u64>,
}

/// Result of one sync pass.
#[derive(Debug, Default)]
pub struct SnipdSyncStats {
    /// No folder path has been configured yet.
    pub no_folder: bool,
    /// Number of new/changed files ingested this pass.
    pub new_files: u64,
}

// ---------------------------------------------------------------------------
// Frontmatter parser
//
// Best-effort YAML frontmatter parser covering the subset Snipd uses:
//   scalar:     `key: value`
//   block list: `key:\n  - item`
//   inline list:`key: [a, b]`
//
// Snipd fields (documented in research notes and plugin source):
//   podcast / show_title  — podcast name
//   episode / episode_title — episode name
//   timestamp / snip_start_time — in-episode timestamp string (e.g. "00:12:34")
//   date / created / episode_publish_date — snip creation / episode date
//   tags — list
//   summary / snip_note — AI summary or user note
//   snips_count — integer (Obsidian plugin, per-episode files)
//   snip_url / episode_url — deep link
//
// Unknown keys are preserved in the raw layer verbatim.

#[derive(Debug, Clone)]
enum FmVal {
    Scalar(String),
    List(Vec<String>),
}

/// Parse the leading `---`…`---` YAML block, returning `(frontmatter, body)`.
fn parse_frontmatter(content: &str) -> (HashMap<String, FmVal>, &str) {
    // Must begin with `---\n` or `---\r\n`.
    let after_open = if let Some(rest) = content.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = content.strip_prefix("---\r\n") {
        rest
    } else {
        return (HashMap::new(), content);
    };

    let close_marker = "\n---";
    let close_pos = match after_open.find(close_marker) {
        Some(p) => p,
        None => return (HashMap::new(), content),
    };

    let fm_block = &after_open[..close_pos];
    let after_close = &after_open[close_pos + close_marker.len()..];
    let body = match after_close.find('\n') {
        Some(nl) => &after_close[nl + 1..],
        None => "",
    };

    (parse_fm_block(fm_block), body)
}

fn parse_fm_block(block: &str) -> HashMap<String, FmVal> {
    let mut map: HashMap<String, FmVal> = HashMap::new();
    let lines: Vec<&str> = block.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            i += 1;
            continue;
        }
        let colon = match line.find(':') {
            Some(p) => p,
            None => { i += 1; continue; }
        };
        let key = line[..colon].trim().to_string();
        if key.is_empty() { i += 1; continue; }
        let raw_val = line[colon + 1..].trim();

        if raw_val.is_empty() {
            // Block list.
            i += 1;
            let mut items = Vec::new();
            while i < lines.len() {
                let l = lines[i].trim();
                if l.starts_with("- ") || l == "-" {
                    let item = l.trim_start_matches('-').trim().to_string();
                    if !item.is_empty() { items.push(item); }
                    i += 1;
                } else if l.is_empty() {
                    i += 1;
                } else {
                    break;
                }
            }
            if !items.is_empty() { map.insert(key, FmVal::List(items)); }
        } else if raw_val.starts_with('[') && raw_val.ends_with(']') {
            // Inline list.
            let inner = &raw_val[1..raw_val.len() - 1];
            let items: Vec<String> = inner
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !items.is_empty() { map.insert(key, FmVal::List(items)); }
            i += 1;
        } else {
            let val = raw_val.trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() { map.insert(key, FmVal::Scalar(val)); }
            i += 1;
        }
    }
    map
}

fn fm_scalar(map: &HashMap<String, FmVal>, key: &str) -> String {
    match map.get(key) {
        Some(FmVal::Scalar(s)) => s.clone(),
        Some(FmVal::List(v)) if v.len() == 1 => v[0].clone(),
        _ => String::new(),
    }
}

// Used in tests and will be used when the per-snip highlight parser is un-parked.
#[allow(dead_code)]
fn fm_list(map: &HashMap<String, FmVal>, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(FmVal::List(v)) => v.clone(),
        Some(FmVal::Scalar(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Serialize the frontmatter map to a JSON object for the raw layer.
fn fm_to_json(map: &HashMap<String, FmVal>) -> Map<String, Value> {
    let mut obj = Map::new();
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort();
    for k in keys {
        let v = match &map[k] {
            FmVal::Scalar(s) => Value::String(s.clone()),
            FmVal::List(items) => Value::Array(items.iter().cloned().map(Value::String).collect()),
        };
        obj.insert(k.to_string(), v);
    }
    obj
}

// ---------------------------------------------------------------------------
// Date helpers

/// Parse Snipd date strings to RFC3339 local time. Accepts ISO-8601, common
/// naive formats (YYYY-MM-DD, YYYY-MM-DDTHH:MM:SS, etc.), and passes through
/// strings it can't parse verbatim (so they still land in the raw layer).
fn parse_snipd_date(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() { return None; }

    // RFC3339 / ISO-8601 with tz.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }

    // Naive formats: try most specific first.
    let naive_fmts = &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ];
    for fmt in naive_fmts {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(local) = Local.from_local_datetime(&dt).earliest() {
                return Some(local.to_rfc3339());
            }
        }
    }

    // Date only → local midnight.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            if let Some(local) = Local.from_local_datetime(&dt).earliest() {
                return Some(local.to_rfc3339());
            }
        }
    }

    None
}

use chrono::TimeZone as _;

/// Unix epoch seconds → RFC3339 local.
fn unix_to_local(secs: u64) -> String {
    let dt = DateTime::<Local>::from(
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
    );
    dt.to_rfc3339()
}

fn mtime_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// File ID

/// Stable vault-relative id from an absolute path under a source folder.
/// Forward-slash normalized, case-preserved.
fn rel_path(folder_root: &Path, path: &Path) -> String {
    path.strip_prefix(folder_root)
        .expect("path is always under folder_root during walk")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Filesystem walk

/// Walk `dir` for `.md` files (non-recursive would miss sub-show-folders).
/// Skips hidden directories (`.*`).
fn walk_for_md(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') { continue; }

        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            walk_for_md(&path, out);
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "md") {
            let mtime = meta.modified().map(mtime_secs).unwrap_or(0);
            out.push((path, mtime));
        }
    }
}

// ---------------------------------------------------------------------------
// Parse one .md file → raw object

/// Parse a Snipd `.md` file into a full-fidelity raw JSON object.
/// Returns `None` if the file cannot be read.
fn parse_md_file(folder_root: &Path, path: &Path, mtime_s: u64) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    let rel = rel_path(folder_root, path);
    let (fm, body) = parse_frontmatter(&content);

    // Best-effort date extraction.
    // Snipd direct export: `date:` or `created:`
    // Obsidian plugin: `episode_publish_date:` — episode-level date
    let raw_ts = fm_scalar(&fm, "date")
        .pipe_if_empty(|| fm_scalar(&fm, "created"))
        .pipe_if_empty(|| fm_scalar(&fm, "episode_publish_date"));

    let ts = if !raw_ts.is_empty() {
        parse_snipd_date(&raw_ts).unwrap_or_else(|| unix_to_local(mtime_s))
    } else {
        unix_to_local(mtime_s)
    };

    // Build the raw object — all frontmatter verbatim + body + path + mtime.
    let mut obj = Map::new();
    obj.insert("source".into(), Value::String(SOURCE.to_string()));
    obj.insert("id".into(), Value::String(rel.clone()));
    obj.insert("path".into(), Value::String(rel.clone()));
    obj.insert("ts".into(), Value::String(ts.clone()));
    // Body verbatim (transcript excerpt, AI summary, user notes may be in here).
    if !body.is_empty() {
        obj.insert("body".into(), Value::String(body.to_string()));
    }
    // Full frontmatter as a sub-object (preserves all keys including
    // Obsidian-plugin-specific ones like snips_count, episode_url, etc.).
    let fm_json = fm_to_json(&fm);
    if !fm_json.is_empty() {
        obj.insert("frontmatter".into(), Value::Object(fm_json));
    }
    obj.insert("file_mtime_secs".into(), Value::from(mtime_s));
    // Partition key for the raw layer (month of ts).
    obj.insert("_ts".into(), Value::String(ts.clone()));

    Some(Value::Object(obj))
}

// Helper trait for readable pipe-if-empty chains.
trait PipeIfEmpty {
    fn pipe_if_empty<F: FnOnce() -> String>(self, f: F) -> String;
}
impl PipeIfEmpty for String {
    fn pipe_if_empty<F: FnOnce() -> String>(self, f: F) -> String {
        if self.is_empty() { f() } else { self }
    }
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One incremental scan of the user's Snipd export folder.
    /// Returns `no_folder: true` if no folder has been configured or the path
    /// doesn't exist. Otherwise scans, ingests new/changed files to the raw
    /// layer, and advances the mtime watermark.
    pub fn collect_snipd(&self) -> Result<SnipdSyncStats> {
        let mut state = self.read_snipd_sync().unwrap_or_default();

        if state.folder_path.is_empty() {
            return Ok(SnipdSyncStats { no_folder: true, ..Default::default() });
        }

        let folder = PathBuf::from(&state.folder_path);
        if !folder.is_dir() {
            return Ok(SnipdSyncStats { no_folder: true, ..Default::default() });
        }

        // Walk for all .md files + mtimes.
        let mut files: Vec<(PathBuf, u64)> = Vec::new();
        walk_for_md(&folder, &mut files);

        // Filter to new or changed files.
        let changed: Vec<(PathBuf, u64)> = files
            .iter()
            .filter(|(path, mtime)| {
                let r = rel_path(&folder, path);
                state.mtimes.get(&r).copied().unwrap_or(0) < *mtime
            })
            .cloned()
            .collect();

        if changed.is_empty() {
            state.updated = Local::now().to_rfc3339();
            self.write_snipd_sync(&state)?;
            return Ok(SnipdSyncStats { no_folder: false, new_files: 0 });
        }

        // Parse and write.
        let mut raw_rows: Vec<Value> = Vec::new();
        for (path, mtime) in &changed {
            if let Some(raw) = parse_md_file(&folder, path, *mtime) {
                let r = rel_path(&folder, path);
                state.mtimes.insert(r, *mtime);
                raw_rows.push(raw);
            }
        }

        let n = raw_rows.len() as u64;
        if !raw_rows.is_empty() {
            self.upsert_snipd_raw(&raw_rows)?;
        }

        state.updated = Local::now().to_rfc3339();
        self.write_snipd_sync(&state)?;
        Ok(SnipdSyncStats { no_folder: false, new_files: n })
    }

    /// Upsert raw rows into `reading/snipd/raw/YYYY-MM.jsonl`, partitioned by
    /// the month of `_ts`, deduped by `id` across ALL partitions.
    ///
    /// Cross-month dedup: when a file's `_ts` shifts to a new month (e.g.
    /// its mtime crosses a month boundary because the Obsidian plugin rewrote
    /// it), we must remove the stale row from the old month's partition before
    /// inserting into the new one. Scanning only the target month's partition
    /// would leave a duplicate in the old month.
    fn upsert_snipd_raw(&self, rows: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_ts").and_then(Value::as_str).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(Value::as_str).unwrap_or("")
        }

        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in rows {
            if let Some(key) = Partition::Month.key(month_of(v)) {
                by_month.entry(key.to_string()).or_default().push(v);
            }
        }

        // All ids being upserted — needed for cross-month stale-row removal.
        let all_incoming_ids: std::collections::HashSet<&str> =
            rows.iter().map(|v| id_of(v)).collect();

        let stream = self.stream(RAW_DIR, Partition::Month);

        // Remove stale rows from ALL existing partitions that are NOT the
        // target month for each id. This handles the case where mtime crossed
        // a month boundary so the file would land in a different partition
        // than it did on the previous ingest.
        let all_partitions = stream.partitions()?;
        for part in &all_partitions {
            if by_month.contains_key(part.as_str()) {
                // The target month is handled in the merge loop below.
                continue;
            }
            // Read this other-month partition and drop any rows whose id is
            // being re-ingested into a different month.
            let existing: Vec<Value> = stream.read::<Value>(part)?;
            let needs_removal = existing.iter().any(|ex| all_incoming_ids.contains(id_of(ex)));
            if needs_removal {
                let pruned: Vec<Value> = existing
                    .into_iter()
                    .filter(|ex| !all_incoming_ids.contains(id_of(ex)))
                    .collect();
                let rel = format!("{RAW_DIR}/{part}.jsonl");
                self.write_snapshot(&rel, &pruned)?;
            }
        }

        // Write each target-month partition with the incoming rows merged in,
        // deduping within the target month as well (handles same-month re-ingest).
        for (month, incoming) in by_month {
            let inc_ids: std::collections::HashSet<&str> =
                incoming.iter().map(|v| id_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|ex| !inc_ids.contains(id_of(ex)))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{RAW_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Read the persisted sync state.
    pub fn read_snipd_sync(&self) -> Option<SnipdSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_snipd_sync(&self, state: &SnipdSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_json_atomic(&path, state)
    }

    /// Set the Snipd export folder path (called from the UI setup flow).
    /// Resets the mtime watermark so all files are re-ingested from the new
    /// folder.
    pub fn set_snipd_folder_path(&self, folder: &str) -> Result<()> {
        let state = SnipdSyncState {
            folder_path: folder.to_string(),
            ..Default::default()
        };
        self.write_snipd_sync(&state)
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-snipd-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a fake Snipd export folder with given files.
    fn fake_snipd_folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("trove-snipd-src-{}-{name}", std::process::id()));
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
    // Snipd direct-export sample (per-snip file, as documented in research
    // notes: podcast, episode, timestamp, tags, summary + body transcript).
    // Field names are from the research docs; exact casing confirmed from
    // integration-research.md.
    //
    // NOTE: This is a synthesized fixture from documented fields. A real
    // sample is needed to confirm exact names and types (hence
    // parser_parked_needs_sample=true). The parser is tolerant: unknown
    // frontmatter keys land in extra via the frontmatter sub-object.

    fn snip_direct_export() -> &'static str {
        r#"---
podcast: The Knowledge Project
episode: Thinking in First Principles with Shane Parrish
date: 2026-05-15T10:30:00+00:00
timestamp: "00:23:45"
tags:
  - mental-models
  - first-principles
summary: Shane discusses how mental models help us think more clearly.
---

## Transcript excerpt

This is the moment where you stop and ask: what do I know to be true here? Not what someone told me, not what I read—what can I derive from first principles?

## Notes

Great reminder to question assumptions before applying frameworks.
"#
    }

    /// Obsidian plugin format (per-episode, contains all snips, snips_count).
    fn snip_obsidian_plugin() -> &'static str {
        r#"---
snips_count: 3
episode_publish_date: 2026-05-10
---

# Naval Ravikant on Building Wealth

## Episode metadata
- Episode title: Naval Ravikant on Building Wealth
- Show: The Tim Ferriss Show
- Episode publish date: 2026-05-10

## Snips

### [First Principles of Wealth](https://snipd.com/snip/abc123)

🎧 00:12:34 - 00:14:22 (1:48)

Specific knowledge can't be taught. If it can be taught, it can be automated.

---
"#
    }

    // -----------------------------------------------------------------------
    // Frontmatter parser tests

    #[test]
    fn parses_direct_export_frontmatter_scalar_and_list() {
        let (fm, body) = parse_frontmatter(snip_direct_export());

        assert_eq!(fm_scalar(&fm, "podcast"), "The Knowledge Project");
        assert_eq!(fm_scalar(&fm, "episode"), "Thinking in First Principles with Shane Parrish");
        assert_eq!(fm_scalar(&fm, "timestamp"), "00:23:45");
        assert_eq!(
            fm_scalar(&fm, "summary"),
            "Shane discusses how mental models help us think more clearly."
        );
        assert_eq!(
            fm_list(&fm, "tags"),
            vec!["mental-models", "first-principles"],
            "block-list tags parsed correctly"
        );
        assert!(!body.is_empty(), "body present after frontmatter");
        assert!(body.contains("Transcript excerpt"), "body contains sections");
    }

    #[test]
    fn parses_obsidian_plugin_frontmatter() {
        let (fm, body) = parse_frontmatter(snip_obsidian_plugin());

        assert_eq!(
            fm_scalar(&fm, "snips_count"),
            "3",
            "snips_count (Obsidian plugin) parsed"
        );
        assert_eq!(fm_scalar(&fm, "episode_publish_date"), "2026-05-10");
        assert!(!body.is_empty(), "body present");
    }

    #[test]
    fn frontmatter_to_json_preserves_all_keys() {
        let (fm, _) = parse_frontmatter(snip_direct_export());
        let j = fm_to_json(&fm);

        assert!(j.contains_key("podcast"), "podcast in json: {:?}", j.keys().collect::<Vec<_>>());
        assert!(j.contains_key("tags"), "tags in json");
        // Tags should be a JSON array.
        assert!(j["tags"].is_array(), "tags serialized as array");
    }

    #[test]
    fn no_frontmatter_returns_empty_map_and_full_body() {
        let content = "# Just a heading\n\nSome content without frontmatter.";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty(), "no frontmatter → empty map");
        assert_eq!(body, content, "whole content is body");
    }

    #[test]
    fn inline_list_in_frontmatter() {
        let content = "---\ntags: [a, b, c]\n---\nbody";
        let (fm, _) = parse_frontmatter(content);
        assert_eq!(fm_list(&fm, "tags"), vec!["a", "b", "c"]);
    }

    // -----------------------------------------------------------------------
    // Date parsing tests

    #[test]
    fn parse_snipd_date_rfc3339() {
        let ts = parse_snipd_date("2026-05-15T10:30:00+00:00").unwrap();
        let epoch = DateTime::parse_from_rfc3339(&ts).unwrap().timestamp();
        // 2026-05-15T10:30:00Z in Unix seconds.
        assert_eq!(epoch, 1_778_841_000, "instant preserved: {ts}");
    }

    #[test]
    fn parse_snipd_date_date_only() {
        let ts = parse_snipd_date("2026-05-15").unwrap();
        // Must parse back to a valid RFC3339.
        assert!(DateTime::parse_from_rfc3339(&ts).is_ok(), "date-only yields valid RFC3339: {ts}");
    }

    #[test]
    fn parse_snipd_date_empty_returns_none() {
        assert!(parse_snipd_date("").is_none());
        assert!(parse_snipd_date("   ").is_none());
    }

    // -----------------------------------------------------------------------
    // Vault integration tests

    #[test]
    fn no_folder_configured_returns_no_folder_stat() {
        let v = temp_vault("nofolder");
        let stats = v.collect_snipd().unwrap();
        assert!(stats.no_folder, "unconfigured → no_folder");
        assert_eq!(stats.new_files, 0);
    }

    #[test]
    fn missing_folder_returns_no_folder_stat() {
        let v = temp_vault("missingfolder");
        v.set_snipd_folder_path("/tmp/nonexistent-snipd-folder-xyz-9999").unwrap();
        let stats = v.collect_snipd().unwrap();
        assert!(stats.no_folder, "non-existent folder → no_folder");
    }

    #[test]
    fn scan_writes_raw_and_advances_cursor() {
        let v = temp_vault("scan");
        let folder = fake_snipd_folder("scan", &[
            ("Knowledge-Project.md", snip_direct_export()),
            ("TimFerriss/Naval-Ravikant.md", snip_obsidian_plugin()),
        ]);

        v.set_snipd_folder_path(folder.to_str().unwrap()).unwrap();
        let stats = v.collect_snipd().unwrap();
        assert!(!stats.no_folder);
        assert_eq!(stats.new_files, 2, "two files ingested: {}", stats.new_files);

        // Raw files should exist under reading/snipd/raw/.
        let raw_dir = v.root().join("reading/snipd/raw");
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") { Some(n) } else { None }
            })
            .collect();
        assert!(!raw_files.is_empty(), "raw JSONL files written: {:?}", raw_files);

        // Verify raw content includes key fields.
        let raw_content: String = raw_files
            .iter()
            .map(|f| fs::read_to_string(v.root().join("reading/snipd/raw").join(f)).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(raw_content.contains("Knowledge-Project.md"), "path in raw: {raw_content}");
        assert!(raw_content.contains("The Knowledge Project"), "podcast name in frontmatter");
        assert!(raw_content.contains("snipd"), "source field present");

        // Cursor advanced, updated time set.
        let state = v.read_snipd_sync().unwrap();
        assert!(!state.updated.is_empty(), "updated timestamp set");
        assert_eq!(state.mtimes.len(), 2, "two files tracked in cursor");

        // Re-run: nothing new.
        let stats2 = v.collect_snipd().unwrap();
        assert_eq!(stats2.new_files, 0, "second run is a no-op");
    }

    #[test]
    fn incremental_sync_picks_up_new_file() {
        let v = temp_vault("incremental");
        let folder = fake_snipd_folder("incremental", &[
            ("first.md", snip_direct_export()),
        ]);
        v.set_snipd_folder_path(folder.to_str().unwrap()).unwrap();
        let s1 = v.collect_snipd().unwrap();
        assert_eq!(s1.new_files, 1);

        // Add a second file.
        fs::write(folder.join("second.md"), snip_obsidian_plugin()).unwrap();
        let s2 = v.collect_snipd().unwrap();
        assert_eq!(s2.new_files, 1, "only the new file is ingested: {:?}", s2.new_files);
    }

    #[test]
    fn raw_dedupes_on_file_id() {
        // Ingesting the same file twice (e.g. if mtime isn't reliably
        // updated) must not duplicate rows.
        let v = temp_vault("dedup");
        let folder = fake_snipd_folder("dedup", &[
            ("snip.md", snip_direct_export()),
        ]);
        v.set_snipd_folder_path(folder.to_str().unwrap()).unwrap();
        v.collect_snipd().unwrap();

        // Force re-ingest by resetting cursor.
        let mut state = v.read_snipd_sync().unwrap();
        state.mtimes.clear();
        v.write_snipd_sync(&state).unwrap();
        v.collect_snipd().unwrap();

        // Check the raw file has exactly 1 line.
        let raw_dir = v.root().join("reading/snipd/raw");
        let mut lines = 0u64;
        for entry in fs::read_dir(&raw_dir).unwrap().flatten() {
            if entry.file_name().to_string_lossy().ends_with(".jsonl") {
                let body = fs::read_to_string(entry.path()).unwrap();
                lines += body.lines().count() as u64;
            }
        }
        assert_eq!(lines, 1, "deduped: still exactly 1 raw line after re-ingest");
    }

    /// Regression test for the cross-month duplicate bug: if a file's _ts moves
    /// to a different month (e.g. because mtime crossed a month boundary), re-
    /// ingesting must leave exactly one row for that id across ALL partitions —
    /// not one in the old month plus one in the new month.
    #[test]
    fn raw_dedupes_across_month_boundary() {

        let v = temp_vault("xmonth");

        // Manually plant a raw row for "episode.md" in the OLD month (2026-04).
        // This simulates a previous ingest when the file's mtime was in April.
        let old_row = json!({
            "source": "snipd",
            "id": "episode.md",
            "path": "episode.md",
            "ts": "2026-04-30T23:50:00+00:00",
            "_ts": "2026-04-30T23:50:00+00:00",
            "body": "old body",
            "file_mtime_secs": 1_000_000u64,
        });
        let old_rel = format!("{RAW_DIR}/2026-04.jsonl");
        v.write_snapshot(&old_rel, &[old_row]).unwrap();

        // Now upsert the same id with a NEW-month _ts (2026-05), simulating
        // mtime having crossed the April→May boundary.
        let new_row = json!({
            "source": "snipd",
            "id": "episode.md",
            "path": "episode.md",
            "ts": "2026-05-01T00:05:00+00:00",
            "_ts": "2026-05-01T00:05:00+00:00",
            "body": "updated body",
            "file_mtime_secs": 2_000_000u64,
        });
        v.upsert_snipd_raw(&[new_row]).unwrap();

        // Count total rows across ALL raw partitions — must be exactly 1.
        let raw_dir = v.root().join("reading/snipd/raw");
        let mut total_lines = 0usize;
        let mut found_ids: Vec<String> = Vec::new();
        if raw_dir.exists() {
            for entry in fs::read_dir(&raw_dir).unwrap().flatten() {
                if entry.file_name().to_string_lossy().ends_with(".jsonl") {
                    let body = fs::read_to_string(entry.path()).unwrap();
                    for line in body.lines().filter(|l| !l.trim().is_empty()) {
                        total_lines += 1;
                        if let Ok(v) = serde_json::from_str::<Value>(line) {
                            if let Some(id) = v.get("id").and_then(Value::as_str) {
                                found_ids.push(id.to_string());
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(
            total_lines, 1,
            "exactly 1 raw row across all partitions after cross-month re-ingest; \
             found {} rows with ids {:?}",
            total_lines, found_ids
        );

        // And it must be in the NEW month (2026-05), not the old one.
        let may_file = v.root().join("reading/snipd/raw/2026-05.jsonl");
        assert!(may_file.exists(), "new-month partition (2026-05) exists");
        let may_content = fs::read_to_string(&may_file).unwrap();
        assert!(may_content.contains("updated body"), "new-month row has updated content");

        // Old month partition must be empty (zero rows).
        let apr_file = v.root().join("reading/snipd/raw/2026-04.jsonl");
        if apr_file.exists() {
            let apr_content = fs::read_to_string(&apr_file).unwrap();
            let apr_lines = apr_content.lines().filter(|l| !l.trim().is_empty()).count();
            assert_eq!(apr_lines, 0, "old-month partition (2026-04) must have 0 rows after prune");
        }
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        v.set_snipd_folder_path("/some/path").unwrap();
        let loaded = v.read_snipd_sync().unwrap();
        assert_eq!(loaded.folder_path, "/some/path");
        assert!(loaded.mtimes.is_empty());
    }

    #[test]
    fn sync_state_back_compat_empty_json() {
        // An empty `{}` must deserialize to all-default (no panics on first
        // run or after the user deletes the cursor file).
        let empty: SnipdSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.folder_path.is_empty());
        assert!(empty.mtimes.is_empty());
        assert!(empty.updated.is_empty());
    }

    #[test]
    fn def_is_periodic_reading_no_connection() {
        assert_eq!(DEF.meta.id, "snipd");
        assert_eq!(DEF.meta.domain, "reading");
        assert!(DEF.connection.is_none(), "no login required");
        assert!(DEF.pull.is_some(), "pull hook present");
        assert!(DEF.last_data.is_some());
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
    }
}
