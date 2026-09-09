//! Reflect Notes — import via export ZIP or individual Markdown files.
//!
//! # What this does
//!
//! Reflect (reflect.app) exports notes as a ZIP of Markdown files (app menu →
//! Settings → Data → Export → "Markdown zip"). The Reflect API is **write-only**
//! (the app is E2EE), so export or local backup is the only read path.
//!
//! # Format (Needs-sample — exact .md layout unconfirmed)
//!
//! Primary sources:
//! - Official team-reflect/reflect-import repo: Convertor interface defines
//!   `subject/html/createdAt/updatedAt/backlinkedNoteIds` — this is the
//!   *import-to-Reflect* data model and demonstrates Reflect's internal schema.
//! - Reflect Academy (reflect.academy/import-export-backups): four export
//!   formats — "Reflect JSON", "Reflect CSV", "Markdown zip", "HTML zip".
//! - Reflect Academy (reflect.academy/using-backlinks-and-tags): backlinks use
//!   `[[Entity Name]]` syntax; tags use `#tagname` inline in the note body.
//!
//! YAML frontmatter presence in Reflect's own Markdown export is **not
//! confirmed**. Reflect Academy's note that importing Obsidian-style frontmatter
//! makes Reflect treat the frontmatter block as the note title strongly suggests
//! Reflect's own model has no frontmatter concept — and therefore its Markdown
//! export is unlikely to emit one.
//!
//! This parser handles BOTH layouts defensively:
//!
//! 1. **Preferred (no-frontmatter, likely real format):** date derived from
//!    filename stem when it matches `YYYY-MM-DD` (Reflect names daily notes by
//!    date). First H1 heading or filename stem is the title. `#tags` and
//!    `[[backlinks]]` extracted inline from the body.
//!
//! 2. **Bonus (frontmatter present):** if a `---`…`---` block is found, fields
//!    `created`/`date`, `updated`/`modified`, `tags`, `title`, `id`/`uuid` are
//!    read from it. The filename-date derivation is still preferred for `created`
//!    when the filename is a date.
//!
//! `parser_parked_needs_sample = true` — get a real export to confirm exact
//! layout, then remove this flag.
//!
//! # Silent-zero detection
//!
//! If a ZIP is imported but contains no `.md` files (HTML-zip or JSON/CSV
//! export), the importer returns an error asking the user to re-export as
//! "Markdown zip" rather than silently reporting 0 notes imported.
//!
//! # Vault layout
//!
//! - Contract: `notes/reflect/YYYY-MM.jsonl` (one [`Note`] per note, partitioned
//!   by `created` month, deduped by `id`)
//! - Raw: `notes/reflect/raw/YYYY-MM.jsonl` (full fidelity, same partitioning)
//!
//! Brief: `docs/integrations/reflect.md`

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "reflect";
const NOTES_DIR: &str = "notes/reflect";
const RAW_DIR: &str = "notes/reflect/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "reflect",
        name: "Reflect",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Reflect notes from an export ZIP \
                      (Markdown files) or from individual Markdown note files. \
                      Re-runnable: re-importing an export never duplicates. \
                      The Reflect API is write-only (E2EE), so export is the \
                      only supported read path. Export as 'Markdown zip' — \
                      HTML/JSON/CSV exports are not accepted.",
        domain: "notes",
        vault_path: "notes/reflect/",
        toggleable: false,
        setup: &[
            "In Reflect: Settings → Data → Export → choose 'Markdown zip'.",
            "Import the downloaded ZIP here (or drop individual .md files).",
        ],
        caveats: "The Reflect API is write-only due to end-to-end encryption; \
                 only the export or backup path is readable. Export as 'Markdown \
                 zip'; HTML/JSON/CSV exports will be rejected with a clear error. \
                 Tags (#tag) and backlinks ([[note]]) are extracted from the note \
                 body inline. Backup folder location is not publicly documented.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "md"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Frontmatter parsing (optional — may not be present in real Reflect exports)
// Handles the same YAML subset as obsidian.rs (single-level scalar + list).

/// A parsed frontmatter value — scalar or list.
#[derive(Debug, Clone)]
enum FmVal {
    Scalar(String),
    List(Vec<String>),
}

/// Parse the leading `---`…`---` YAML frontmatter block.
/// Returns `(map, body_after_delimiter)`. If no valid block exists, the whole
/// content is the body and the map is empty.
fn parse_frontmatter(content: &str) -> (HashMap<String, FmVal>, &str) {
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
    let body = if let Some(nl) = after_close.find('\n') {
        &after_close[nl + 1..]
    } else {
        ""
    };

    let map = parse_fm_block(fm_block);
    (map, body)
}

fn parse_fm_block(block: &str) -> HashMap<String, FmVal> {
    let mut map: HashMap<String, FmVal> = HashMap::new();
    let lines: Vec<&str> = block.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            i += 1;
            continue;
        }
        let colon = match line.find(':') {
            Some(p) => p,
            None => {
                i += 1;
                continue;
            }
        };
        let key = line[..colon].trim().to_string();
        if key.is_empty() {
            i += 1;
            continue;
        }
        let raw_val = line[colon + 1..].trim();

        if raw_val.is_empty() {
            // Block list.
            i += 1;
            let mut items = Vec::new();
            while i < lines.len() {
                let l = lines[i].trim();
                if l.starts_with("- ") || l == "-" {
                    let item = l.trim_start_matches('-').trim().to_string();
                    if !item.is_empty() {
                        items.push(item);
                    }
                    i += 1;
                } else if l.is_empty() {
                    i += 1;
                } else {
                    break;
                }
            }
            if !items.is_empty() {
                map.insert(key, FmVal::List(items));
            }
        } else if raw_val.starts_with('[') && raw_val.ends_with(']') {
            let inner = &raw_val[1..raw_val.len() - 1];
            let items: Vec<String> = inner
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !items.is_empty() {
                map.insert(key, FmVal::List(items));
            }
            i += 1;
        } else {
            let val = raw_val.trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() {
                map.insert(key, FmVal::Scalar(val));
            }
            i += 1;
        }
    }
    map
}

fn fm_scalar(map: &HashMap<String, FmVal>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(FmVal::Scalar(s)) => Some(s.clone()),
        Some(FmVal::List(v)) if v.len() == 1 => Some(v[0].clone()),
        _ => None,
    }
}

fn fm_list(map: &HashMap<String, FmVal>, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(FmVal::List(v)) => v.clone(),
        Some(FmVal::Scalar(s)) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![s.clone()]
            }
        }
        None => vec![],
    }
}

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
// Body-level extraction
//
// Reflect uses #hashtags and [[wikilinks]] inline in the note body (confirmed
// via reflect.academy/using-backlinks-and-tags). When no frontmatter supplies
// tags/backlinks, we extract them from the body text.

/// Extract `#tagname` tokens from the note body (lowercase, no leading `#`).
/// Skips `#heading` lines (Markdown headings) to avoid false positives.
fn extract_body_tags(body: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in body.lines() {
        let trimmed = line.trim();
        // Skip Markdown heading lines — `# Heading` is not a tag.
        if trimmed.starts_with('#') && !trimmed.starts_with("##") {
            // single # heading: skip
            let after = trimmed.trim_start_matches('#').trim();
            if !after.is_empty() && !after.starts_with(' ') {
                // Not a space-separated heading — could be a tag
                // (bare `#tag` at start of line). Fall through.
            } else {
                continue;
            }
        }
        // Find all `#word` tokens in the line.
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '#' {
                // Must be preceded by whitespace or start of line.
                let preceded_by_space = i == 0 || chars[i - 1].is_whitespace();
                if preceded_by_space {
                    let start = i + 1;
                    let end = chars[start..]
                        .iter()
                        .position(|c| !c.is_alphanumeric() && *c != '-' && *c != '_')
                        .map(|p| start + p)
                        .unwrap_or(chars.len());
                    if end > start {
                        let tag: String = chars[start..end].iter().collect();
                        // Filter out pure-digit sequences (e.g. `#123` issue refs).
                        if !tag.chars().all(|c| c.is_ascii_digit()) {
                            let lower = tag.to_lowercase();
                            if seen.insert(lower.clone()) {
                                tags.push(lower);
                            }
                        }
                        i = end;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }
    tags
}

/// Extract `[[link target]]` tokens from the note body.
fn extract_body_backlinks(body: &str) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut remaining = body;
    while let Some(open) = remaining.find("[[") {
        remaining = &remaining[open + 2..];
        if let Some(close) = remaining.find("]]") {
            let target = remaining[..close].trim().to_string();
            if !target.is_empty() && seen.insert(target.clone()) {
                links.push(target);
            }
            remaining = &remaining[close + 2..];
        } else {
            break;
        }
    }
    links
}

// ---------------------------------------------------------------------------
// Date parsing
//
// Priority:
//   1. Filename stem that matches YYYY-MM-DD (Reflect daily notes are named by
//      date — this is the most reliable source when frontmatter is absent).
//   2. Frontmatter `created` / `date` field (if frontmatter is present).
//   3. Fallback to `fallback_ts` (import time).

fn parse_reflect_date(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 1. RFC3339 / ISO-8601 with timezone.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // 2. Naive formats (no tz assumed → local).
    let naive_fmts: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ];
    for fmt in naive_fmts {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Local.from_local_datetime(&dt).earliest()?.to_rfc3339());
        }
    }
    // 3. Date only → local midnight.
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0)?;
        return Some(Local.from_local_datetime(&dt).earliest()?.to_rfc3339());
    }
    None
}

/// Try to derive a date from a filename stem like `2024-03-14` or
/// `2024-03-14-some-suffix`. Returns `None` if the stem doesn't start with a
/// valid `YYYY-MM-DD` prefix.
fn date_from_filename_stem(stem: &str) -> Option<String> {
    // The stem may be a plain path component from a ZIP (e.g. "2024-03-14" or
    // "notes/2024-03-14-weekly"). Grab just the last segment.
    let base = stem.rsplit('/').next().unwrap_or(stem);
    // Extract the YYYY-MM-DD prefix (first 10 chars).
    if base.len() >= 10 {
        parse_reflect_date(&base[..10])
    } else {
        None
    }
}

fn now_local_rfc3339() -> String {
    Local::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Single note parser

/// Parse one `.md` file body + stable `file_id` (filename stem or path-relative
/// id for notes extracted from a ZIP).
///
/// Returns `(contract_note, raw_object)` or `None` if unprocessable.
///
/// ## Format handling
///
/// **No-frontmatter path (likely real Reflect export format):**
/// - Date: derived from `file_id` when it matches `YYYY-MM-DD[…]`. Daily notes
///   in Reflect are named by date (e.g. `2024-03-14.md`).
/// - Title: first `# Heading` in body, or file stem.
/// - Tags: `#tagname` tokens extracted inline from the body.
/// - Backlinks: `[[target]]` tokens extracted inline from the body.
///
/// **Frontmatter present (bonus, unconfirmed format):**
/// - All of the above, supplemented by frontmatter fields where present.
/// - `id`/`uuid` frontmatter → stable dedupe key (overrides file stem).
/// - Filename-date is still preferred over `created` frontmatter for daily notes.
///
/// ## Needs-sample
/// The exact Reflect Markdown export format is not officially documented. This
/// parser is built from: (1) reflect.academy confirming `[[wikilinks]]` and
/// `#hashtags`; (2) team-reflect/reflect-import confirming the internal data
/// model; (3) Reflect Academy noting Reflect treats imported frontmatter AS the
/// note title — implying Reflect's own export likely has none.
fn parse_note(file_id: &str, content: &str, fallback_ts: &str) -> Option<(Note, Value)> {
    let (fm, body) = parse_frontmatter(content);

    // Stable note id: prefer frontmatter `id`/`uuid`; fall back to file stem.
    let id = fm_scalar(&fm, "id")
        .or_else(|| fm_scalar(&fm, "uuid"))
        .unwrap_or_else(|| file_id.to_string());

    // Date priority:
    //   1. Filename stem matching YYYY-MM-DD (most reliable for daily notes).
    //   2. Frontmatter `created` / `date`.
    //   3. fallback_ts (import time).
    let created = date_from_filename_stem(file_id)
        .or_else(|| {
            fm_scalar(&fm, "created")
                .or_else(|| fm_scalar(&fm, "date"))
                .and_then(|s| parse_reflect_date(&s))
        })
        .unwrap_or_else(|| fallback_ts.to_string());

    let modified = fm_scalar(&fm, "updated")
        .or_else(|| fm_scalar(&fm, "modified"))
        .and_then(|s| parse_reflect_date(&s))
        .unwrap_or_else(|| created.clone());

    // Title: frontmatter `title` → first `# Heading` in body → file stem.
    let title = fm_scalar(&fm, "title")
        .filter(|t| !t.is_empty())
        .or_else(|| {
            body.lines()
                .map(str::trim)
                .find(|l| l.starts_with("# "))
                .and_then(|l| l.strip_prefix("# "))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| {
            // Use just the last path component for the title when file_id is a path.
            file_id
                .rsplit('/')
                .next()
                .unwrap_or(file_id)
                .to_string()
        });

    // Tags: frontmatter `tags` field first; supplement/replace with body inline
    // `#tag` tokens when frontmatter has none (the expected Reflect layout).
    let fm_tags: Vec<String> = fm_list(&fm, "tags")
        .into_iter()
        .map(|t| t.strip_prefix('#').map(str::to_string).unwrap_or(t))
        .filter(|t| !t.is_empty())
        .collect();
    let tags = if fm_tags.is_empty() {
        // Extract #hashtags from body (confirmed Reflect inline syntax).
        extract_body_tags(body)
    } else {
        fm_tags
    };

    // Backlinks: frontmatter `backlinks` field first; supplement with body
    // `[[wikilinks]]` when frontmatter has none.
    let fm_backlinks: Vec<String> = fm_list(&fm, "backlinks");
    let backlinks = if fm_backlinks.is_empty() {
        // Extract [[wikilinks]] from body (confirmed Reflect inline syntax).
        extract_body_backlinks(body)
    } else {
        fm_backlinks
    };

    // Extra: backlinks + any unmapped frontmatter fields.
    let contract_keys: HashSet<&str> = [
        "id", "uuid", "title", "created", "date", "updated", "modified", "tags", "backlinks",
    ]
    .into();
    let fm_json = fm_to_json(&fm);
    let mut extra = Map::new();
    for (k, v) in &fm_json {
        if !contract_keys.contains(k.as_str()) {
            extra.insert(k.clone(), v.clone());
        }
    }
    if !backlinks.is_empty() {
        extra.insert(
            "backlinks".into(),
            Value::Array(backlinks.iter().cloned().map(Value::String).collect()),
        );
    }
    if !fm_json.is_empty() {
        extra.insert("frontmatter".into(), Value::Object(fm_json.clone()));
    }

    let mut note = Note::new(SOURCE, &id);
    note.title = title.clone();
    note.body = body.to_string();
    note.created = created.clone();
    note.modified = modified.clone();
    note.tags = tags.clone();
    note.extra = extra;

    // Raw layer (full fidelity).
    let mut raw = Map::new();
    raw.insert("source".into(), Value::String(SOURCE.to_string()));
    raw.insert("id".into(), Value::String(id.clone()));
    raw.insert("file_id".into(), Value::String(file_id.to_string()));
    raw.insert("title".into(), Value::String(title));
    raw.insert("body".into(), Value::String(body.to_string()));
    raw.insert("created".into(), Value::String(created.clone()));
    raw.insert("modified".into(), Value::String(modified));
    if !tags.is_empty() {
        raw.insert(
            "tags".into(),
            Value::Array(tags.into_iter().map(Value::String).collect()),
        );
    }
    if !backlinks.is_empty() {
        raw.insert(
            "backlinks".into(),
            Value::Array(backlinks.into_iter().map(Value::String).collect()),
        );
    }
    if !fm_json.is_empty() {
        raw.insert("frontmatter".into(), Value::Object(fm_json));
    }
    // Immutable partition key for raw upsert.
    raw.insert("_created".into(), Value::String(created));

    Some((note, Value::Object(raw)))
}

// ---------------------------------------------------------------------------
// ZIP extraction

/// Extract notes from a Reflect export ZIP. Each `.md` file at any depth in
/// the ZIP becomes one note. The file's path within the ZIP (`.md` extension
/// stripped) is the stable `file_id`.
///
/// # Format detection / error on non-Markdown exports
///
/// Reflect exports four formats: "Markdown zip", "HTML zip", "Reflect JSON",
/// "Reflect CSV". Only the "Markdown zip" is accepted here. If the ZIP contains
/// no `.md` files but does contain `.html`, `.json`, or `.csv` files, the
/// function returns an error guiding the user to re-export as "Markdown zip"
/// rather than silently reporting 0 notes imported.
///
/// # ZIP structure (Needs-sample; format not officially documented)
/// Expected to be flat or one-level deep; we walk all entries regardless.
fn notes_from_zip(path: &Path) -> Result<Vec<(Note, Value)>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", path.display()))?;

    let now = now_local_rfc3339();
    let mut out = Vec::new();
    let mut non_md_extensions: HashSet<String> = HashSet::new();

    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let entry_name = entry.name().to_string();

        // Skip directories and macOS metadata.
        if entry_name.ends_with('/') || entry_name.starts_with("__MACOSX/") {
            continue;
        }

        if !entry_name.ends_with(".md") {
            // Track non-md extension for silent-zero detection.
            if let Some(ext) = entry_name.rfind('.').map(|p| &entry_name[p + 1..]) {
                let lower = ext.to_lowercase();
                if matches!(lower.as_str(), "html" | "json" | "csv") {
                    non_md_extensions.insert(lower);
                }
            }
            continue;
        }

        let mut content = String::new();
        if entry.read_to_string(&mut content).is_err() {
            continue;
        }
        let file_id = entry_name
            .strip_suffix(".md")
            .unwrap_or(&entry_name)
            .to_string();
        if let Some(pair) = parse_note(&file_id, &content, &now) {
            out.push(pair);
        }
    }

    // Silent-zero detection: if we found no Markdown notes but did find HTML/JSON/CSV,
    // the user almost certainly imported the wrong export format.
    if out.is_empty() && !non_md_extensions.is_empty() {
        let found: Vec<_> = non_md_extensions.into_iter().collect();
        anyhow::bail!(
            "This ZIP appears to be a '{}' export, not a 'Markdown zip'. \
             Please re-export from Reflect (Settings → Data → Export) and choose \
             'Markdown zip', then import the new file.",
            found.join("/")
        );
    }

    Ok(out)
}

/// Parse a single `.md` file (non-ZIP import path).
fn notes_from_md(path: &Path) -> Result<Vec<(Note, Value)>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let now = now_local_rfc3339();
    let file_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let pairs = parse_note(&file_id, &content, &now)
        .map(|p| vec![p])
        .unwrap_or_default();
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// Import runner

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let pairs = if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        notes_from_zip(path)?
    } else {
        notes_from_md(path)?
    };

    // Dedupe against already-stored notes (re-runnable imports).
    let stream = vault.stream(NOTES_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for note in stream.read::<Note>(&key)? {
            if !note.id.is_empty() {
                seen.insert(note.id.clone());
            }
        }
    }

    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    let mut contract: Vec<Note> = Vec::new();
    let mut raw: Vec<Value> = Vec::new();

    for (note, raw_row) in pairs {
        if note.created.is_empty() {
            skipped += 1;
            continue;
        }
        if !seen.insert(note.id.clone()) {
            duplicates += 1;
            continue;
        }
        contract.push(note);
        raw.push(raw_row);
        imported += 1;
        if imported % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    if !contract.is_empty() {
        vault.upsert_reflect_notes(&contract)?;
        vault.upsert_reflect_raw(&raw)?;
    }

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} notes imported, {duplicates} duplicates skipped"),
        counts: [("imported", imported), ("duplicates", duplicates), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// Vault helpers (same pattern as evernote.rs)

impl Vault {
    /// Upsert Reflect contract notes into `notes/reflect/YYYY-MM.jsonl`,
    /// partitioned by `created` month, deduped by `id`.
    pub fn upsert_reflect_notes(&self, notes: &[Note]) -> Result<()> {
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
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
                .filter(|e| !incoming_ids.contains(e.id.as_str()))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{NOTES_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Upsert raw rows into `notes/reflect/raw/YYYY-MM.jsonl`, partitioned
    /// by `_created`, deduped by `id`.
    pub fn upsert_reflect_raw(&self, rows: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
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
                .filter(|e| !incoming_ids.contains(id_of(e)))
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
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-reflect-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Unit tests: parse_note (no-frontmatter path — likely real Reflect format)

    /// Minimal note: no frontmatter, just a heading body. Date falls back to
    /// fallback_ts since file stem has no date component.
    #[test]
    fn parse_note_no_frontmatter() {
        let content = "# My first note\n\nSome content here.";
        let now = "2026-06-01T10:00:00-07:00";
        let (note, raw) = parse_note("my-first-note", content, now).unwrap();
        assert_eq!(note.source, "reflect");
        assert_eq!(note.id, "my-first-note");
        assert_eq!(note.title, "My first note");
        assert_eq!(note.created, now);
        assert!(note.tags.is_empty());
        assert_eq!(raw["id"], "my-first-note");
        assert_eq!(raw["title"], "My first note");
    }

    /// Daily note with a date-named filename (the likely real Reflect format).
    /// Date is derived from the filename stem, NOT from frontmatter or fallback.
    #[test]
    fn parse_note_date_filename_no_frontmatter() {
        let content = "# Daily Note\n\nSome thoughts for the day. #work #ideas\n\n[[Project Alpha]] meeting today.";
        let fallback = "2026-06-01T10:00:00-07:00";
        let (note, raw) = parse_note("2026-03-14", content, fallback).unwrap();
        assert!(
            note.created.starts_with("2026-03-14"),
            "date derived from filename, got: {}",
            note.created
        );
        assert_eq!(note.title, "Daily Note");
        // Tags extracted from body inline.
        assert!(note.tags.contains(&"work".to_string()), "got: {:?}", note.tags);
        assert!(note.tags.contains(&"ideas".to_string()), "got: {:?}", note.tags);
        // Backlinks extracted from body inline.
        assert!(
            note.extra.get("backlinks").is_some(),
            "backlinks in extra: {:?}",
            note.extra
        );
        let bl = note.extra["backlinks"].as_array().unwrap();
        assert!(bl.iter().any(|v| v.as_str() == Some("Project Alpha")));
        assert_eq!(raw["_created"], note.created);
    }

    /// Filename-date takes priority over fallback_ts even when frontmatter is absent.
    #[test]
    fn parse_note_filename_date_beats_fallback() {
        let content = "Just a note body, no heading, no frontmatter.";
        let fallback = "2026-06-01T10:00:00-07:00";
        let (note, _raw) = parse_note("2025-11-07", content, fallback).unwrap();
        assert!(
            note.created.starts_with("2025-11-07"),
            "filename date used, got: {}",
            note.created
        );
    }

    /// Filename-date takes priority over frontmatter `created` too (daily notes).
    #[test]
    fn parse_note_filename_date_beats_frontmatter_date() {
        let content =
            "---\ncreated: 2026-06-01T10:00:00-07:00\n---\nSome content.";
        let fallback = "2026-01-01T00:00:00-08:00";
        // A date-named file — the filename date should win.
        let (note, _raw) = parse_note("2025-03-20", content, fallback).unwrap();
        assert!(
            note.created.starts_with("2025-03-20"),
            "filename date beats frontmatter, got: {}",
            note.created
        );
    }

    /// Full frontmatter: created, updated, tags, backlinks, id.
    /// (Kept to ensure the frontmatter path still works for any exports that do
    /// have frontmatter.)
    #[test]
    fn parse_note_full_frontmatter() {
        let content = "\
---
id: abc-123-def
title: Project ideas
created: 2026-03-14T09:00:00-07:00
updated: 2026-04-01T18:00:00-07:00
tags:
  - projects
  - ideas
backlinks:
  - daily/2026-03-14
---
# Project ideas

Body text here.
";
        let now = "2026-06-01T10:00:00-07:00";
        // Non-date filename: frontmatter `created` is used (no filename-date).
        let (note, raw) = parse_note("project-ideas", content, now).unwrap();
        assert_eq!(note.id, "abc-123-def", "frontmatter id preferred over file stem");
        assert_eq!(note.title, "Project ideas");
        assert_eq!(note.created, "2026-03-14T09:00:00-07:00");
        assert_eq!(note.modified, "2026-04-01T18:00:00-07:00");
        assert_eq!(note.tags, vec!["projects", "ideas"]);
        assert!(note.extra.contains_key("backlinks"), "backlinks in extra");
        assert!(note.extra.contains_key("frontmatter"), "full frontmatter in extra");
        assert_eq!(raw["id"], "abc-123-def");
        assert_eq!(raw["_created"], "2026-03-14T09:00:00-07:00");
    }

    /// Tags as an inline YAML list (frontmatter path).
    #[test]
    fn parse_note_inline_tags() {
        let content =
            "---\ntags: [rust, programming]\ncreated: 2026-05-01T08:00:00-07:00\n---\nBody.";
        let now = "2026-06-01T10:00:00-07:00";
        let (note, _raw) = parse_note("rust-notes", content, now).unwrap();
        assert_eq!(note.tags, vec!["rust", "programming"]);
    }

    /// Tags with leading # are stripped in frontmatter.
    #[test]
    fn parse_note_tag_hash_stripped() {
        let content =
            "---\ntags:\n  - #garden\n  - spring\ncreated: 2026-04-01T09:00:00-07:00\n---\n";
        let now = "2026-06-01T10:00:00-07:00";
        let (note, _raw) = parse_note("garden-notes", content, now).unwrap();
        assert_eq!(note.tags, vec!["garden", "spring"]);
    }

    /// Date-only `created` field in frontmatter → local midnight.
    /// (Only used when filename is not date-named.)
    #[test]
    fn parse_note_date_only_created() {
        let content = "---\ncreated: 2026-06-10\n---\nBody.";
        let now = "2026-06-01T10:00:00-07:00";
        // Non-date filename: frontmatter `created` used.
        let (note, raw) = parse_note("undated-note", content, now).unwrap();
        assert!(note.created.starts_with("2026-06-10"), "got: {}", note.created);
        assert_eq!(raw["_created"], note.created);
    }

    /// No `created` in frontmatter and non-date filename → falls back to fallback_ts.
    #[test]
    fn parse_note_fallback_ts_when_no_created() {
        let content = "---\ntags: [test]\n---\nBody.";
        let fallback = "2026-01-15T12:00:00-08:00";
        let (note, _) = parse_note("no-date-note", content, fallback).unwrap();
        assert_eq!(note.created, fallback);
    }

    // ---------------------------------------------------------------------------
    // Body tag/backlink extraction tests

    /// Inline `#tag` tokens in the body are extracted when no frontmatter tags.
    #[test]
    fn extract_body_tags_basic() {
        let tags = extract_body_tags("Some text #work and #ideas here.");
        assert!(tags.contains(&"work".to_string()));
        assert!(tags.contains(&"ideas".to_string()));
    }

    /// Markdown heading lines (`# Heading`) are not treated as tags.
    #[test]
    fn extract_body_tags_ignores_headings() {
        let tags = extract_body_tags("# My Heading\n\nSome text #actual-tag below.");
        assert!(!tags.contains(&"my".to_string()), "heading wrongly extracted");
        assert!(tags.contains(&"actual-tag".to_string()));
    }

    /// `[[wikilinks]]` in the body are extracted as backlinks.
    #[test]
    fn extract_body_backlinks_basic() {
        let links = extract_body_backlinks("See [[Project Alpha]] and [[Daily Notes]].");
        assert_eq!(links, vec!["Project Alpha", "Daily Notes"]);
    }

    /// Duplicate backlinks are deduplicated.
    #[test]
    fn extract_body_backlinks_dedup() {
        let links = extract_body_backlinks("See [[Alpha]] and also [[Alpha]] again.");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "Alpha");
    }

    // ---------------------------------------------------------------------------
    // Integration tests: full vault round-trip

    fn write_zip(dir: &std::path::Path, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let zip_path = dir.join("reflect-export.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(f);
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        zip_path
    }

    fn do_import(vault: &Vault, path: &std::path::Path) -> ImportOutcome {
        (IMPORT.run)(vault, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Test the likely real Reflect format: date-named file, no frontmatter,
    /// inline #tags and [[backlinks]].
    #[test]
    fn imports_date_named_note_no_frontmatter() {
        let v = temp_vault("zip-date-named");
        let content = "# Daily Note\n\nReflecting on today. #project #review\n\nSee [[Weekly Review]] for more.";
        let zip = write_zip(v.root(), &[("2026-05-15.md", content)]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 1);

        let notes: Vec<Note> = v.stream(NOTES_DIR, Partition::Month).read("2026-05").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, "2026-05-15", "file stem is id when no frontmatter id");
        assert_eq!(notes[0].title, "Daily Note");
        assert!(notes[0].created.starts_with("2026-05-15"), "date from filename");
        assert!(notes[0].tags.contains(&"project".to_string()));
        assert!(notes[0].tags.contains(&"review".to_string()));
        let bl = notes[0].extra.get("backlinks")
            .and_then(|v| v.as_array())
            .expect("backlinks in extra");
        assert!(bl.iter().any(|v| v.as_str() == Some("Weekly Review")));
    }

    #[test]
    fn imports_note_from_zip() {
        let v = temp_vault("zip-basic");
        let content = "\
---
id: note-001
title: My Test Note
created: 2026-05-15T10:00:00-07:00
updated: 2026-05-20T14:00:00-07:00
tags:
  - test
  - trove
---
# My Test Note

This is the body.
";
        let zip = write_zip(v.root(), &[("my-test-note.md", content)]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 1);
        assert_eq!(out.counts["duplicates"], 0);

        // Contract layer.
        let notes: Vec<Note> = v.stream(NOTES_DIR, Partition::Month).read("2026-05").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, "note-001");
        assert_eq!(notes[0].title, "My Test Note");
        assert_eq!(notes[0].tags, vec!["test", "trove"]);
        assert_eq!(notes[0].created, "2026-05-15T10:00:00-07:00");
        assert_eq!(notes[0].modified, "2026-05-20T14:00:00-07:00");

        // Raw layer.
        let raw: Vec<Value> = v.stream(RAW_DIR, Partition::Month).read("2026-05").unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0]["id"], "note-001");
        assert_eq!(raw[0]["_created"], "2026-05-15T10:00:00-07:00");
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("zip-idempotent");
        let content =
            "---\nid: note-002\ncreated: 2026-04-10T08:00:00-07:00\n---\nBody.";
        let zip = write_zip(v.root(), &[("note-002.md", content)]);
        let out1 = do_import(&v, &zip);
        assert_eq!(out1.counts["imported"], 1);
        let out2 = do_import(&v, &zip);
        assert_eq!(out2.counts["imported"], 0);
        assert_eq!(out2.counts["duplicates"], 1);

        let notes: Vec<Note> = v.stream(NOTES_DIR, Partition::Month).read("2026-04").unwrap();
        assert_eq!(notes.len(), 1, "no duplicates after re-import");
    }

    #[test]
    fn imports_multiple_notes_across_months() {
        let v = temp_vault("zip-multi-month");
        // Use date-named files (the likely real format) — no frontmatter.
        let jan = "# January Note\n\nJanuary content here.";
        let mar = "# March Note\n\nMarch content here.";
        let zip = write_zip(v.root(), &[("2026-01-10.md", jan), ("2026-03-15.md", mar)]);
        let out = do_import(&v, &zip);
        assert_eq!(out.counts["imported"], 2);

        let jan_notes: Vec<Note> =
            v.stream(NOTES_DIR, Partition::Month).read("2026-01").unwrap();
        let mar_notes: Vec<Note> =
            v.stream(NOTES_DIR, Partition::Month).read("2026-03").unwrap();
        assert_eq!(jan_notes.len(), 1);
        assert_eq!(mar_notes.len(), 1);
        assert_eq!(jan_notes[0].id, "2026-01-10");
        assert_eq!(mar_notes[0].id, "2026-03-15");
        assert!(jan_notes[0].created.starts_with("2026-01-10"));
        assert!(mar_notes[0].created.starts_with("2026-03-15"));
    }

    #[test]
    fn imports_single_md_file() {
        let v = temp_vault("md-single");
        let md_path = v.root().join("my-note.md");
        fs::write(
            &md_path,
            "---\nid: single-001\ncreated: 2026-06-01T09:00:00-07:00\ntags: [single]\n---\n# Hello\n\nWorld.",
        )
        .unwrap();
        let out = do_import(&v, &md_path);
        assert_eq!(out.counts["imported"], 1);
        let notes: Vec<Note> = v.stream(NOTES_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(notes[0].id, "single-001");
        assert_eq!(notes[0].tags, vec!["single"]);
    }

    #[test]
    fn skips_macos_metadata_entries() {
        let v = temp_vault("zip-macos");
        let content = "# Real Note\n\nContent here.";
        let zip_path = v.root().join("export.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("2026-05-01.md", opts).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
        writer.start_file("__MACOSX/._2026-05-01.md", opts).unwrap();
        writer.write_all(b"garbage").unwrap();
        writer.finish().unwrap();

        let out = do_import(&v, &zip_path);
        assert_eq!(out.counts["imported"], 1, "macOS metadata skipped");
    }

    /// HTML-zip export (wrong format) → error, not silent zero.
    #[test]
    fn html_zip_returns_error_not_silent_zero() {
        let v = temp_vault("zip-html");
        let zip_path = v.root().join("html-export.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("note1.html", opts).unwrap();
        writer.write_all(b"<html><body>content</body></html>").unwrap();
        writer.start_file("note2.html", opts).unwrap();
        writer.write_all(b"<html><body>more</body></html>").unwrap();
        writer.finish().unwrap();

        let result = notes_from_zip(&zip_path);
        assert!(result.is_err(), "HTML zip should return an error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Markdown zip"),
            "error should mention Markdown zip, got: {msg}"
        );
    }

    /// JSON export (wrong format) → error, not silent zero.
    #[test]
    fn json_zip_returns_error_not_silent_zero() {
        let v = temp_vault("zip-json");
        let zip_path = v.root().join("json-export.zip");
        let f = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("notes.json", opts).unwrap();
        writer.write_all(b"[{\"subject\":\"test\"}]").unwrap();
        writer.finish().unwrap();

        let result = notes_from_zip(&zip_path);
        assert!(result.is_err(), "JSON zip should return an error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Markdown zip"),
            "error should mention Markdown zip, got: {msg}"
        );
    }

    /// Empty ZIP (no files at all) → Ok(empty), not an error (no false positive).
    #[test]
    fn empty_zip_is_ok_not_error() {
        let v = temp_vault("zip-empty");
        let zip_path = v.root().join("empty.zip");
        let f = fs::File::create(&zip_path).unwrap();
        zip::ZipWriter::new(f).finish().unwrap();

        let result = notes_from_zip(&zip_path);
        assert!(result.is_ok(), "empty zip should be Ok, not an error");
        assert_eq!(result.unwrap().len(), 0);
    }
}
