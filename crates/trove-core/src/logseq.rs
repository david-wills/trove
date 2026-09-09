//! Logseq — import of a Logseq graph as a zipped folder of Markdown files.
//!
//! **Graph format (official docs):** A Logseq graph is a plain folder on disk
//! containing:
//!   - `pages/<PageName>.md` — regular knowledge-base pages
//!   - `journals/YYYY_MM_DD.md` — daily journal entries, one per day
//!   - `.logseq/` — internal config/cache (skipped entirely)
//!   - `.bak/` — auto-backup files (skipped)
//!
//! Each `.md` file is plain Markdown (or optionally `.org`; this import
//! handles `.md` only — `.org` is skipped). Block-level outline syntax
//! (`- item`) and `[[wikilinks]]` / `#tags` are preserved verbatim in `body`
//! (Logseq's block structure is an in-app view, not the on-disk contract; the
//! full text is the durable record). `.org` support is a Needs-David followup.
//!
//! **Import:** user zips their graph root and drops the ZIP here. The import
//! walks every `.md` file at depth ≤ 2 under the ZIP root, classifying by
//! subfolder (`journals/` vs `pages/` vs root). Each file becomes one
//! [`Note`] in the `notes` contract:
//!   - `guid` = the graph-relative path, namespace-decoded (e.g.
//!     `pages/Garden planting plan.md`, or `pages/BJJ/chokes/triangle.md`
//!     for a namespaced page stored as `BJJ___chokes___triangle.md` or
//!     `BJJ%2Fchokes%2Ftriangle.md`), used as the stable dedupe key across
//!     re-imports. The raw on-disk path is preserved in `extra.logseq_path`.
//!   - `title` = for pages: the decoded page name (namespace `/`-separated,
//!     e.g. `BJJ/chokes/triangle`), derived from the filename; for journals:
//!     the date formatted `YYYY-MM-DD`; for journals only, first `# ` H1
//!     overrides the date stem when present
//!   - `body` = full file content, verbatim (trim trailing whitespace only)
//!   - `created` = for journals: midnight local on `YYYY-MM-DD` from the
//!     filename; for pages: the ZIP entry's last-modified timestamp (coarse
//!     DOS-time, 2-second resolution), filed in `modified` in the contract;
//!     falls back to import time only when the ZIP carries no mtime
//!   - `modified` = ZIP entry's last-modified timestamp for all files (carried
//!     in raw and forwarded to the contract note's `modified` when present)
//!   - `folder` = `"journals"` or `"pages"` (or `""` for root-level files)
//!   - `extra.logseq_path` = the original (raw, un-decoded) path within the
//!     ZIP for full-fidelity reference
//!
//! **Namespace encoding:** Logseq encodes the `/` namespace separator in
//! page filenames. Graphs created on Logseq ≤ 0.8.8 (`:file/name-format
//! :legacy`) use percent-encoding (`%2F`); graphs created on ≥ 0.8.9 (the
//! default) use triple-lowbar (`___`). Both are decoded to `/` before the
//! title and guid are set, so the contract layer always sees the human page
//! name with `/`-separated hierarchy. The raw on-disk filename is preserved
//! in `extra.logseq_path`.
//!
//! Re-importing a newer ZIP upserts by `guid` — no duplicates.
//!
//! **macOS lazy-flush caveat:** Logseq had a bug (#10510, 2024) where edits
//! were not written to disk promptly. The setup copy reminds users to verify
//! their graph is in on-disk mode before exporting.
//!
//! **Catalogued:** docs/integrations/logseq.md — Phase 2 brief. Behavior
//! upgraded from `NotWired` to `Import` (the Periodic/folder-watch future
//! is gated on a folder-picker Tauri command; zipped import is immediately
//! usable and covers the same data).

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "logseq";
const NOTES_DIR: &str = "notes/logseq";
const RAW_DIR: &str = "notes/logseq/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "logseq",
        name: "Logseq",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Logseq graph — pages and daily journal files — \
                      from a ZIP of your graph folder. Each page and journal entry \
                      lands in the notes stream with full body text preserved. \
                      Re-importing a newer ZIP updates notes in place without \
                      creating duplicates.",
        domain: "notes",
        vault_path: "notes/logseq/",
        toggleable: false,
        setup: &[
            "Confirm Logseq is in on-disk mode (Graphs panel → graph → check \
             the folder path is a real local path, not a Logseq DB mode graph).",
            "Zip your graph folder: right-click the graph folder in Finder → \
             Compress. The ZIP must contain the graph root (pages/ and journals/ \
             at the top level inside the ZIP or one level in).",
            "Import the ZIP here. Re-import at any time to update notes in place.",
        ],
        caveats: "Only .md files are collected; .org files are skipped. A known \
                 Logseq bug (#10510, 2024) may delay the latest edits until the \
                 app is restarted — confirm on-disk mode before exporting. Block \
                 references ([[wikilinks]] and #tags) are preserved verbatim in \
                 the note body.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// File classification

/// Where in the graph a `.md` file lives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileKind {
    Journal,
    Page,
    Root,
}

/// One Markdown file from the graph ZIP, ready to parse.
struct GraphFile {
    /// Path within the ZIP archive (e.g. `mygraph/pages/Planning.md`), raw and un-decoded.
    zip_path: String,
    /// Path relative to the graph root, namespace-decoded (e.g.
    /// `pages/Planning.md` or `pages/BJJ/chokes/triangle.md`).
    graph_path: String,
    kind: FileKind,
    /// Filename without extension, namespace-decoded (e.g. `Planning`,
    /// `BJJ/chokes/triangle`, or `2026_06_15`).
    stem: String,
    body: String,
    /// ZIP entry last-modified time as RFC3339, if the archive carries one.
    /// DOS-time has 2-second resolution and no timezone; stored as local time.
    zip_mtime: Option<String>,
}

/// Decode Logseq's namespace encoding in a page file stem.
///
/// Logseq encodes the `/` namespace separator differently depending on the
/// graph's `:file/name-format` setting:
///   - `:triple-lowbar` (default since Logseq 0.8.9): `___` → `/`
///   - `:legacy` (older graphs): `%2F` → `/` (percent-encoding)
///
/// Journals and root files are not namespaced; their stems pass through
/// unchanged. Both encodings are decoded so the contract layer always uses
/// the human page name with `/`-separated hierarchy.
fn decode_logseq_namespace(stem: &str) -> String {
    // Triple-lowbar is the modern default; check it first.
    if stem.contains("___") {
        return stem.replace("___", "/");
    }
    // Legacy percent-encoding for the namespace separator.
    if stem.contains("%2F") || stem.contains("%2f") {
        // Replace case-insensitively (both %2F and %2f appear in the wild).
        let s = stem.replace("%2f", "/");
        return s.replace("%2F", "/");
    }
    stem.to_string()
}

/// Classify a file that lives at `graph_path` within the graph root.
/// `zip_path` is the original (raw, un-decoded) path in the archive.
/// `zip_mtime` is the ZIP entry's last-modified timestamp as RFC3339, if any.
fn classify_at_graph_path(
    zip_path: &str,
    graph_path: String,
    body: String,
    zip_mtime: Option<String>,
) -> Option<GraphFile> {
    // Skip hidden/internal folders.
    if graph_path.starts_with(".logseq/")
        || graph_path.starts_with(".bak/")
        || graph_path.contains("/.logseq/")
        || graph_path.contains("/.bak/")
        || graph_path.starts_with('.')
    {
        return None;
    }

    // Determine raw stem (filename without .md), then decode namespace encoding.
    let raw_stem = Path::new(&graph_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())?;

    // Classify by subfolder — classify before decoding so prefix check is stable.
    let kind = if graph_path.starts_with("journals/") {
        FileKind::Journal
    } else if graph_path.starts_with("pages/") {
        FileKind::Page
    } else if !graph_path.contains('/') {
        // Root-level .md file (e.g. `contents.md` in older Logseq layouts).
        FileKind::Root
    } else {
        // Deeper nesting (user sub-folders) — treat as a page.
        FileKind::Page
    };

    // Decode namespace encoding for pages and root files. Journals use a
    // date stem (YYYY_MM_DD) which never contains `___` or `%2F`.
    let stem = match kind {
        FileKind::Page | FileKind::Root => decode_logseq_namespace(&raw_stem),
        FileKind::Journal => raw_stem,
    };

    // Build the decoded graph_path: replace the raw stem with the decoded stem
    // in the path so the guid is also namespace-decoded.
    let decoded_graph_path = if stem != Path::new(&graph_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
    {
        // Stem changed — rebuild path with decoded stem + .md extension.
        let parent = Path::new(&graph_path)
            .parent()
            .map(|p| p.to_string_lossy())
            .unwrap_or_default();
        if parent.is_empty() || parent == "." {
            format!("{stem}.md")
        } else {
            format!("{parent}/{stem}.md")
        }
    } else {
        graph_path
    };

    Some(GraphFile {
        zip_path: zip_path.to_string(),
        graph_path: decoded_graph_path,
        kind,
        stem,
        body,
        zip_mtime,
    })
}

// ---------------------------------------------------------------------------
// Date parsing

/// Parse a Logseq journal stem `YYYY_MM_DD` to a local midnight RFC3339
/// string. Returns `None` if the stem doesn't match the pattern.
fn parse_journal_date(stem: &str) -> Option<String> {
    // Standard format: 2026_06_15 → 2026-06-15
    let s = stem.replace('_', "-");
    let date = NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()?;
    // Local midnight.
    let dt = Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest()?;
    Some(dt.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Body parsing helpers

/// Extract the title: the text of the first `# ` H1 heading in the body,
/// if present. Falls back to `None` (callers use the stem/date).
///
/// Skips leading blank lines and Logseq property lines (`key:: value`) before
/// giving up. Returns `None` if the first non-blank, non-property line is not
/// an H1.
fn extract_h1(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        // Skip blank lines.
        if trimmed.is_empty() {
            continue;
        }
        // Skip Logseq property lines: `key:: value` (key is word characters,
        // `::` separator, value follows — typical Logseq front-matter block).
        if is_logseq_property(trimmed) {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        // First non-blank, non-property line is not an H1 — give up.
        break;
    }
    None
}

/// Returns `true` if `line` is a Logseq property line (`key:: value`).
/// Logseq property keys are word-like identifiers followed by `::`.
fn is_logseq_property(line: &str) -> bool {
    // Fast path: must contain `::`
    if let Some(idx) = line.find("::") {
        // The key portion (before `::`) must be non-empty and contain only
        // word characters, hyphens, or forward slashes (namespaced properties).
        let key = &line[..idx];
        !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '/')
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Collect all .md files from the ZIP.
    let files = read_graph_files(path)?;
    let total = files.len() as u64;
    if total == 0 {
        return Ok(ImportOutcome {
            headline: "No Markdown files found in the ZIP.".into(),
            counts: [("imported", 0), ("updated", 0), ("skipped", 0)].into(),
        });
    }

    // Load existing guids for re-import dedup detection.
    let stream = vault.stream(NOTES_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for note in stream.read::<Note>(&key)? {
            if !note.id.is_empty() {
                seen.insert(note.id.clone());
            }
        }
    }

    let (mut imported, mut updated, mut skipped) = (0u64, 0u64, 0u64);
    let mut contract: Vec<Note> = Vec::new();
    let mut raw: Vec<Value> = Vec::new();

    for (idx, file) in files.into_iter().enumerate() {
        // guid = namespace-decoded graph-relative path (stable across re-imports).
        let guid = file.graph_path.clone();
        let is_update = seen.contains(&guid);
        seen.insert(guid.clone());

        // Determine created timestamp and title.
        // For pages: title is always the decoded filename stem (the authoritative
        // Logseq page name); H1 overrides are intentionally NOT applied to pages
        // because the app renders the page name from the filename, not from any
        // in-body heading. H1 override applies to journals only (where the date
        // stem is the fallback).
        let (created, title) = match file.kind {
            FileKind::Journal => {
                let ts = match parse_journal_date(&file.stem) {
                    Some(ts) => ts,
                    None => {
                        skipped += 1;
                        continue; // can't file without a date
                    }
                };
                // Journal title: prefer H1, fall back to the date string.
                let t = extract_h1(&file.body)
                    .unwrap_or_else(|| file.stem.replace('_', "-"));
                (Some(ts), t)
            }
            FileKind::Page | FileKind::Root => {
                // Title is the decoded page name from the filename — authoritative.
                // Do NOT override with H1 (that heading belongs to page content,
                // not the page identity).
                let t = file.stem.clone();
                (None, t)
            }
        };

        // Body: full content, trim trailing whitespace lines.
        let body = file.body.trim_end().to_string();

        // Contract note.
        let mut note = Note::new(SOURCE, &guid);
        note.title = title.clone();
        note.body = body.clone();
        if let Some(ts) = &created {
            note.created = ts.clone();
        }
        // Set the contract `modified` field from the ZIP entry mtime when
        // available (DOS-time coarse, but real). Also use it as the page
        // creation fallback for partitioning when no journal date is present.
        if let Some(mtime) = &file.zip_mtime {
            note.modified = mtime.clone();
            if note.created.is_empty() {
                // Use file mtime as created proxy for pages.
                note.created = mtime.clone();
            }
        }
        note.folder = match file.kind {
            FileKind::Journal => "journals".to_string(),
            FileKind::Page => "pages".to_string(),
            FileKind::Root => String::new(),
        };
        let mut extra = Map::new();
        // logseq_path holds the original (raw, un-decoded) ZIP path for
        // full-fidelity reference; guid/title carry the decoded form.
        extra.insert("logseq_path".into(), Value::String(file.zip_path.clone()));
        note.extra = extra;

        // Pages without any timestamp (ZIP carries no mtime) fall back to
        // import date for partitioning. This is the last resort only.
        if note.created.is_empty() {
            note.created = Local::now().to_rfc3339();
        }

        // Raw row: full fidelity.
        let mut raw_obj = Map::new();
        raw_obj.insert("source".into(), Value::String(SOURCE.into()));
        raw_obj.insert("id".into(), Value::String(guid.clone()));
        raw_obj.insert("title".into(), Value::String(title));
        raw_obj.insert("body".into(), Value::String(body));
        raw_obj.insert("zip_path".into(), Value::String(file.zip_path));
        raw_obj.insert("graph_path".into(), Value::String(file.graph_path));
        raw_obj.insert(
            "kind".into(),
            Value::String(match file.kind {
                FileKind::Journal => "journal".into(),
                FileKind::Page => "page".into(),
                FileKind::Root => "root".into(),
            }),
        );
        raw_obj.insert("stem".into(), Value::String(file.stem));
        if let Some(ts) = &created {
            raw_obj.insert("created".into(), Value::String(ts.clone()));
        }
        if let Some(mtime) = &file.zip_mtime {
            raw_obj.insert("modified".into(), Value::String(mtime.clone()));
        }
        // Immutable partition key for the raw layer (mirrors contract).
        raw_obj.insert("_created".into(), Value::String(note.created.clone()));
        raw.push(Value::Object(raw_obj));

        contract.push(note);

        if is_update {
            updated += 1;
        } else {
            imported += 1;
        }
        if (idx as u64 + 1) % 100 == 0 {
            let pct = (idx as f32 + 1.0) / total.max(1) as f32 * 90.0;
            progress(ImportProgress { records: imported + updated, percent: pct });
        }
    }

    if !contract.is_empty() {
        vault.upsert_logseq_notes(&contract)?;
        vault.upsert_logseq_raw(&raw)?;
    }
    progress(ImportProgress { records: imported + updated, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} notes imported, {updated} updated, {skipped} skipped"
        ),
        counts: [
            ("imported", imported),
            ("updated", updated),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Detect whether all ZIP entries share a single common top-level folder
/// (e.g., `mygraph/pages/…` and `mygraph/journals/…` → prefix `mygraph/`).
/// Returns the prefix to strip (with trailing slash), or `""` if paths are
/// already at the graph root.
///
/// The heuristic: collect the first path component of every entry; if there
/// is exactly ONE distinct first component AND none of the paths start with
/// `pages/`, `journals/`, or `.logseq/` at the root, that component is the
/// archive's outer wrapper folder.
fn detect_zip_prefix(names: &[String]) -> String {
    let mut first_components: std::collections::HashSet<String> = HashSet::new();
    let mut has_direct_graph_paths = false;
    for name in names {
        let norm = name.replace('\\', "/");
        if norm.ends_with('/') {
            continue;
        }
        if norm.starts_with("pages/")
            || norm.starts_with("journals/")
            || norm.starts_with(".logseq/")
        {
            has_direct_graph_paths = true;
        }
        if let Some(comp) = norm.splitn(2, '/').next() {
            first_components.insert(comp.to_string());
        }
    }
    if has_direct_graph_paths || first_components.len() != 1 {
        return String::new();
    }
    // All paths share one top-level folder: strip it.
    let folder = first_components.into_iter().next().unwrap_or_default();
    if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    }
}

/// Read all `.md` files from a Logseq graph ZIP, returning classified entries.
/// Unknown paths (non-`.md`, `.logseq/`, `.bak/`) are silently skipped —
/// a Logseq graph ZIP has many such files and skipping is the right default.
fn read_graph_files(path: &Path) -> Result<Vec<GraphFile>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", path.display()))?;
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();

    // Detect whether the ZIP has an outer wrapper folder.
    let prefix = detect_zip_prefix(&names);

    let mut out: Vec<GraphFile> = Vec::new();
    for name in &names {
        // Directories have trailing slashes; skip them.
        if name.ends_with('/') || name.ends_with('\\') {
            continue;
        }
        // Only .md files (after normalizing separators).
        let norm = name.replace('\\', "/");
        if !norm.ends_with(".md") {
            continue;
        }

        // Strip the detected prefix to get the graph-relative path.
        let graph_path = if !prefix.is_empty() && norm.starts_with(&prefix) {
            norm[prefix.len()..].to_string()
        } else {
            norm.clone()
        };

        let mut entry = match archive.by_name(name) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // Read the ZIP entry mtime while the entry is borrowed. zip DateTime
        // returns year/month/day/hour/minute/second directly; the `chrono`
        // feature is not enabled in our dependency, so we use those accessors
        // to build a NaiveDateTime manually.
        let zip_mtime: Option<String> = entry.last_modified().and_then(|dt| {
            let naive = chrono::NaiveDate::from_ymd_opt(
                dt.year().into(),
                dt.month().into(),
                dt.day().into(),
            )
            .and_then(|d| d.and_hms_opt(dt.hour().into(), dt.minute().into(), dt.second().into()));
            naive.map(|ndt| {
                // Treat ZIP mtime as local time (DOS timestamps are local,
                // unzoned). Convert to a local-offset RFC3339 string.
                Local
                    .from_local_datetime(&ndt)
                    .earliest()
                    .map(|ldt| ldt.to_rfc3339())
                    .unwrap_or_else(|| ndt.and_utc().to_rfc3339())
            })
        });
        let mut body = String::new();
        if entry.read_to_string(&mut body).is_err() {
            // Non-UTF-8 content (rare in markdown but possible); skip.
            continue;
        }
        drop(entry);

        if let Some(gf) = classify_at_graph_path(name, graph_path, body, zip_mtime) {
            out.push(gf);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Vault impl — upsert helpers (same snapshot+upsert pattern as day_one.rs)

impl Vault {
    /// Upsert contract notes into `notes/logseq/YYYY-MM.jsonl`, partitioned
    /// by the month of `created`, deduped by `id`. Each affected month file
    /// is read whole, lines with a matching `id` are replaced, and the file
    /// is rewritten atomically.
    pub fn upsert_logseq_notes(&self, notes: &[Note]) -> Result<()> {
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            let Some(key) = Partition::Month.key(&n.created) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(n);
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|n| n.id.as_str()).collect();
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

    /// Upsert raw rows into `notes/logseq/raw/YYYY-MM.jsonl`, partitioned by
    /// the month of `_created` (immutable), deduped by `id`.
    fn upsert_logseq_raw(&self, rows: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in rows {
            let Some(key) = Partition::Month.key(month_of(v)) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(v);
        }
        let stream = self.stream(RAW_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|v| id_of(v)).collect();
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
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write as _;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-logseq-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a synthetic Logseq graph ZIP. Entries are `(path_in_zip, content)`.
    fn make_zip(v: &Vault, name: &str, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let zip_path = v.root().join(format!("{name}.zip"));
        let mut w =
            zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (path, content) in entries {
            w.start_file(*path, opts).unwrap();
            w.write_all(content.as_bytes()).unwrap();
        }
        w.finish().unwrap();
        zip_path
    }

    fn do_import(v: &Vault, path: &std::path::PathBuf) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Collect all contract notes across all partitions (pages may land in a
    /// different month than journals when the ZIP carries a real or default mtime).
    fn all_notes(v: &Vault) -> Vec<Note> {
        let stream = v.stream(NOTES_DIR, Partition::Month);
        let mut out = Vec::new();
        for key in stream.partitions().unwrap_or_default() {
            if let Ok(rows) = stream.read::<Note>(&key) {
                out.extend(rows);
            }
        }
        out
    }

    /// Collect all raw rows across all partitions.
    fn all_raw(v: &Vault) -> Vec<Value> {
        let stream = v.stream(RAW_DIR, Partition::Month);
        let mut out = Vec::new();
        for key in stream.partitions().unwrap_or_default() {
            if let Ok(rows) = stream.read::<Value>(&key) {
                out.extend(rows);
            }
        }
        out
    }

    // ---------------------------------------------------------------------------
    // Unit: classify_at_graph_path and detect_zip_prefix

    #[test]
    fn classify_journal_page_root_and_skips_logseq_folder() {
        // classify_at_graph_path receives the already-stripped graph-relative path.
        let j = classify_at_graph_path(
            "mygraph/journals/2026_06_15.md",
            "journals/2026_06_15.md".into(),
            "body".into(),
            None,
        )
        .unwrap();
        assert_eq!(j.kind, FileKind::Journal);
        assert_eq!(j.graph_path, "journals/2026_06_15.md");
        assert_eq!(j.stem, "2026_06_15");

        let p = classify_at_graph_path(
            "mygraph/pages/My Page.md",
            "pages/My Page.md".into(),
            "body".into(),
            None,
        )
        .unwrap();
        assert_eq!(p.kind, FileKind::Page);
        assert_eq!(p.graph_path, "pages/My Page.md");
        assert_eq!(p.stem, "My Page");

        // Root-level file: no slash in graph_path.
        let r = classify_at_graph_path(
            "mygraph/contents.md",
            "contents.md".into(),
            "body".into(),
            None,
        )
        .unwrap();
        assert_eq!(r.kind, FileKind::Root);

        // .logseq/ is internal — skip.
        assert!(classify_at_graph_path(
            "mygraph/.logseq/config.edn",
            ".logseq/config.edn".into(),
            "{}".into(),
            None,
        )
        .is_none());
        // .bak/ is backup — skip.
        assert!(classify_at_graph_path(
            "mygraph/.bak/journals/2026_06_15.md",
            ".bak/journals/2026_06_15.md".into(),
            "".into(),
            None,
        )
        .is_none());
        // Non-.md are filtered before classify_at_graph_path, but if passed
        // with a non-.md path, the stem won't contain "md" — but the function
        // doesn't check extension directly; it's the caller's responsibility.
        // Just ensure a valid graph_path without .md extension still works
        // (yields a valid struct — the extension check is upstream).
    }

    // ---------------------------------------------------------------------------
    // Unit: decode_logseq_namespace

    #[test]
    fn decode_namespace_triple_lowbar_modern_default() {
        // Modern Logseq (≥ 0.8.9): triple-lowbar is the default encoding.
        assert_eq!(decode_logseq_namespace("BJJ___chokes___triangle"), "BJJ/chokes/triangle");
        assert_eq!(decode_logseq_namespace("A___B___C___D"), "A/B/C/D");
        // Single segment — no encoding — passes through unchanged.
        assert_eq!(decode_logseq_namespace("Planning"), "Planning");
    }

    #[test]
    fn decode_namespace_legacy_percent_encoding() {
        // Legacy Logseq (:file/name-format :legacy): percent-encoding.
        assert_eq!(decode_logseq_namespace("BJJ%2Fchokes%2Ftriangle"), "BJJ/chokes/triangle");
        // Case-insensitive: %2f and %2F both appear in the wild.
        assert_eq!(decode_logseq_namespace("A%2fB%2FC"), "A/B/C");
    }

    #[test]
    fn decode_namespace_plain_page_unchanged() {
        // Normal page names with no namespace separators.
        assert_eq!(decode_logseq_namespace("Garden Planning"), "Garden Planning");
        assert_eq!(decode_logseq_namespace("2026_06_15"), "2026_06_15");
        assert_eq!(decode_logseq_namespace(""), "");
    }

    #[test]
    fn detect_zip_prefix_strips_single_top_level_folder() {
        let names: Vec<String> = vec![
            "mygraph/".to_string(),
            "mygraph/journals/2026_06_15.md".to_string(),
            "mygraph/pages/Planning.md".to_string(),
            "mygraph/contents.md".to_string(),
            "mygraph/.logseq/config.edn".to_string(),
        ];
        assert_eq!(detect_zip_prefix(&names), "mygraph/");

        // No common prefix (already at root).
        let flat: Vec<String> = vec![
            "journals/2026_06_15.md".to_string(),
            "pages/Planning.md".to_string(),
        ];
        assert_eq!(detect_zip_prefix(&flat), "");

        // Multiple top-level components → no prefix.
        let multi: Vec<String> = vec![
            "g1/journals/2026_06_15.md".to_string(),
            "g2/pages/note.md".to_string(),
        ];
        assert_eq!(detect_zip_prefix(&multi), "");
    }

    // ---------------------------------------------------------------------------
    // Unit: parse_journal_date

    #[test]
    fn parse_journal_date_standard_format() {
        let ts = parse_journal_date("2026_06_15").unwrap();
        // Must be an RFC3339 local timestamp starting 2026-06-15.
        assert!(ts.starts_with("2026-06-15"), "got: {ts}");
        assert!(ts.contains('T'), "must be full RFC3339: {ts}");
    }

    #[test]
    fn parse_journal_date_rejects_bad_stems() {
        assert!(parse_journal_date("My Page").is_none());
        assert!(parse_journal_date("2026_06").is_none());
        assert!(parse_journal_date("").is_none());
    }

    // ---------------------------------------------------------------------------
    // Unit: extract_h1

    #[test]
    fn extract_h1_finds_first_heading() {
        assert_eq!(extract_h1("# My Title\n\n- block"), Some("My Title".into()));
        assert_eq!(extract_h1("  # Indented heading\n"), Some("Indented heading".into()));
        assert_eq!(extract_h1("- block\n# Late heading"), None, "body starts with block");
        assert_eq!(extract_h1(""), None);
        assert_eq!(extract_h1("## Sub only"), None, "## is not H1");
    }

    // ---------------------------------------------------------------------------
    // Integration: import a graph ZIP

    /// Minimal real-shaped Logseq graph:
    /// - A journal for 2026-06-15 (dated note, markdown body)
    /// - A page "Garden Planning" (undated page with H1 title)
    /// - A root-level contents.md
    /// - .logseq/config.edn (must be skipped)
    /// - .bak file (must be skipped)
    const JOURNAL_BODY: &str =
        "- Morning standup\n- Reviewed [[Garden Planning]] notes\n- #productivity";
    const PAGE_BODY: &str =
        "# Garden Planning\n\n- Tomatoes in the south bed\n- [[Water]] twice a week";
    const ROOT_BODY: &str =
        "- [[Garden Planning]]\n- [[2026-06-15]]";

    fn standard_zip(v: &Vault) -> std::path::PathBuf {
        make_zip(v, "mygraph", &[
            ("mygraph/journals/2026_06_15.md", JOURNAL_BODY),
            ("mygraph/pages/Garden Planning.md", PAGE_BODY),
            ("mygraph/contents.md", ROOT_BODY),
            ("mygraph/.logseq/config.edn", "{:meta/version 1}"),
            ("mygraph/.bak/journals/2026_06_15.md", "old backup"),
        ])
    }

    #[test]
    fn imports_journal_page_root_skips_logseq_and_bak() {
        let v = temp_vault("basic");
        let zip = standard_zip(&v);
        let out = do_import(&v, &zip);

        // 3 notes: journal + page + root contents; .logseq and .bak skipped.
        assert_eq!(out.counts["imported"], 3, "headline: {}", out.headline);
        assert_eq!(out.counts["skipped"], 0);
        assert_eq!(out.counts["updated"], 0);

        // Search across all partitions: journal lands in 2026-06, pages/root
        // may land in the ZIP-mtime month (1980-01 for the default zip DateTime).
        let all = all_notes(&v);
        assert_eq!(all.len(), 3, "3 total notes across all partitions");

        // Journal note: keyed by date, folder=journals, title=date stem.
        let journal = all.iter().find(|n| n.id.contains("journals/"))
            .expect("journal note not found");
        assert_eq!(journal.source, "logseq");
        assert_eq!(journal.folder, "journals");
        assert!(journal.created.starts_with("2026-06-15"), "journal date: {}", journal.created);
        assert!(journal.body.contains("Morning standup"), "body preserved verbatim");
        assert!(journal.body.contains("#productivity"), "inline tags preserved");
        assert!(journal.body.contains("[[Garden Planning]]"), "wikilinks preserved");
        // Journal title: the date stem (no H1 in JOURNAL_BODY).
        assert_eq!(journal.title, "2026-06-15");

        // Page note: folder=pages, title from decoded filename stem (not H1).
        // For pages the authoritative name is the filename; in-body H1 is not
        // used as the title because Logseq renders the page name from the file.
        let page = all.iter().find(|n| n.id.contains("pages/"))
            .expect("page note not found");
        assert_eq!(page.folder, "pages");
        assert_eq!(page.title, "Garden Planning", "page title from filename stem");
        assert!(page.body.contains("Tomatoes"), "full body preserved");

        // Root note.
        let root = all.iter().find(|n| n.id == "contents.md")
            .expect("root note not found");
        assert_eq!(root.folder, "");
        assert!(root.body.contains("[[Garden Planning]]"));

        // extra.logseq_path preserved.
        assert!(journal.extra.contains_key("logseq_path"), "logseq_path in extra");
    }

    #[test]
    fn reimport_updates_in_place_no_duplicates() {
        let v = temp_vault("reimport");
        let zip = standard_zip(&v);

        let first = do_import(&v, &zip);
        assert_eq!(first.counts["imported"], 3);

        // Re-import: same ZIP → all updated, no new rows.
        let second = do_import(&v, &zip);
        assert_eq!(second.counts["imported"], 0, "no new notes on re-import");
        assert_eq!(second.counts["updated"], 3, "all 3 updated in place");
        assert_eq!(second.counts["skipped"], 0);

        // Notes may spread across partitions (journal in 2026-06, pages in
        // the ZIP-mtime month). Count across all.
        let all = all_notes(&v);
        assert_eq!(all.len(), 3, "exactly 3 notes total after re-import");
    }

    #[test]
    fn journal_with_h1_uses_h1_as_title() {
        let v = temp_vault("h1journal");
        let body = "# Daily reflection\n\n- Learned Rust today";
        let zip = make_zip(&v, "g", &[("g/journals/2026_05_01.md", body)]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 1);
        let may = v.stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2026-05").unwrap();
        assert_eq!(may[0].title, "Daily reflection", "H1 overrides date stem for journal");
        assert!(may[0].created.starts_with("2026-05-01"));
    }

    #[test]
    fn invalid_journal_filename_skipped() {
        // A file in journals/ whose name isn't a valid date is skipped
        // (can't be partitioned without a date).
        let v = temp_vault("badjournal");
        let zip = make_zip(&v, "g", &[
            ("g/journals/README.md", "not a date"),
            ("g/pages/Valid Page.md", "content"),
        ]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 1, "only the valid page imported");
        assert_eq!(out.counts["skipped"], 1, "README.md in journals/ skipped");
    }

    #[test]
    fn raw_layer_full_fidelity() {
        let v = temp_vault("raw");
        let zip = standard_zip(&v);
        do_import(&v, &zip);

        // Raw rows may span multiple partitions (journal in 2026-06, pages in
        // the ZIP-mtime month — 1980-01 for the default zip DateTime).
        let all_r = all_raw(&v);
        assert_eq!(all_r.len(), 3, "3 raw rows total across all partitions");

        let journal_raw = all_r.iter()
            .find(|r| r["kind"].as_str() == Some("journal"))
            .expect("journal raw row not found");
        assert_eq!(journal_raw["stem"].as_str(), Some("2026_06_15"));
        assert!(journal_raw["body"].as_str().unwrap().contains("Morning standup"));
        assert!(journal_raw["_created"].as_str().unwrap().starts_with("2026-06-15"));

        let page_raw = all_r.iter()
            .find(|r| r["kind"].as_str() == Some("page"))
            .expect("page raw row not found");
        assert_eq!(page_raw["title"].as_str(), Some("Garden Planning"));
        assert_eq!(page_raw["graph_path"].as_str(), Some("pages/Garden Planning.md"));
    }

    #[test]
    fn empty_zip_returns_zero_counts() {
        let v = temp_vault("empty");
        let zip = make_zip(&v, "g", &[]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 0);
        assert_eq!(out.headline, "No Markdown files found in the ZIP.");
    }

    #[test]
    fn serde_back_compat_old_note_lines_deserialize() {
        let v = temp_vault("backcompat");
        let dir = v.root().join(NOTES_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("2026-06.jsonl"),
            "{\"source\":\"logseq\",\"id\":\"pages/old-page.md\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "pages/old-page.md");
    }

    #[test]
    fn def_is_import_behavior() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.meta.id, "logseq");
        assert_eq!(DEF.meta.domain, "notes");
        assert_eq!(IMPORT.accepts, &["zip"]);
    }

    // ---------------------------------------------------------------------------
    // Namespace decoding: modern (triple-lowbar) and legacy (percent-encoded)

    /// A namespaced page `BJJ/chokes/triangle` stored with triple-lowbar
    /// encoding (the default for Logseq ≥ 0.8.9).
    #[test]
    fn namespaced_page_triple_lowbar_decoded() {
        let v = temp_vault("ns_tribar");
        // On-disk the file is stored as BJJ___chokes___triangle.md
        let zip = make_zip(&v, "g", &[
            ("g/pages/BJJ___chokes___triangle.md", "- arm triangle\n- rear naked choke"),
            ("g/journals/2026_06_10.md", "- trained today"),
        ]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 2);

        // Find the notes written (page is partitioned by zip mtime or fallback).
        let mut found = false;
        let stream = v.stream(NOTES_DIR, Partition::Month);
        for key in stream.partitions().unwrap() {
            for note in stream.read::<Note>(&key).unwrap() {
                if note.id.contains("BJJ") {
                    // Title must be the decoded page name, not the raw filename.
                    assert_eq!(note.title, "BJJ/chokes/triangle",
                        "triple-lowbar stem decoded to '/'-separated title");
                    // guid must also be decoded.
                    assert_eq!(note.id, "pages/BJJ/chokes/triangle.md",
                        "guid uses decoded path");
                    // extra.logseq_path holds the original raw zip path.
                    let raw_path = note.extra.get("logseq_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert!(raw_path.contains("BJJ___chokes___triangle"),
                        "raw zip path preserved in extra: {raw_path}");
                    found = true;
                }
            }
        }
        assert!(found, "namespaced triple-lowbar page must be found in vault");
    }

    /// A namespaced page `BJJ/chokes/triangle` stored with legacy percent-encoding
    /// (`:file/name-format :legacy` in logseq.edn, pre-0.8.9 graphs).
    #[test]
    fn namespaced_page_legacy_percent_encoded_decoded() {
        let v = temp_vault("ns_pct");
        // On-disk the file is stored as BJJ%2Fchokes%2Ftriangle.md
        let zip = make_zip(&v, "g", &[
            ("g/pages/BJJ%2Fchokes%2Ftriangle.md", "- arm triangle\n- rear naked choke"),
        ]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 1);

        let mut found = false;
        let stream = v.stream(NOTES_DIR, Partition::Month);
        for key in stream.partitions().unwrap() {
            for note in stream.read::<Note>(&key).unwrap() {
                if note.id.contains("BJJ") {
                    assert_eq!(note.title, "BJJ/chokes/triangle",
                        "percent-encoded stem decoded to '/'-separated title");
                    assert_eq!(note.id, "pages/BJJ/chokes/triangle.md",
                        "guid uses decoded path");
                    let raw_path = note.extra.get("logseq_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert!(raw_path.contains("BJJ%2Fchokes%2Ftriangle"),
                        "raw zip path preserved in extra: {raw_path}");
                    found = true;
                }
            }
        }
        assert!(found, "namespaced legacy-encoded page must be found in vault");
    }

    /// Pages must NOT override title with an in-body H1 heading.
    /// A page named `Recipes` whose first block is `# Pancakes` must be
    /// titled `Recipes`, not `Pancakes`.
    #[test]
    fn page_title_not_overridden_by_h1() {
        let v = temp_vault("no_h1_override");
        let body = "# Pancakes\n\n- flour 2 cups\n- eggs 2";
        let zip = make_zip(&v, "g", &[
            ("g/pages/Recipes.md", body),
        ]);
        do_import(&v, &zip);

        let mut found = false;
        let stream = v.stream(NOTES_DIR, Partition::Month);
        for key in stream.partitions().unwrap() {
            for note in stream.read::<Note>(&key).unwrap() {
                if note.folder == "pages" {
                    assert_eq!(note.title, "Recipes",
                        "page title must be the filename stem, not the in-body H1");
                    assert!(note.body.contains("# Pancakes"),
                        "H1 heading preserved in body");
                    found = true;
                }
            }
        }
        assert!(found, "page note must be present");
    }

    /// extract_h1 must skip Logseq property lines before an H1.
    #[test]
    fn extract_h1_skips_property_lines() {
        // A journal whose front-matter has property lines before the H1.
        let body = "tags:: zettelkasten\npublic:: true\n# Real Title\n\n- block";
        assert_eq!(extract_h1(body), Some("Real Title".into()),
            "H1 found after property lines");

        // No H1 — body starts with a block item after properties.
        let body2 = "tags:: x\n- block item";
        assert_eq!(extract_h1(body2), None, "no H1 after property + block");
    }
}
