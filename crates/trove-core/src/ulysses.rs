//! Ulysses — import from a Markdown export ZIP.
//!
//! # Export paths
//!
//! Ulysses supports three export modes:
//!
//! 1. **Markdown export** (`File → Export → Markdown`): produces a folder of
//!    `.md` files mirroring the group hierarchy. The user zips this folder and
//!    drops the ZIP here.
//! 2. **TextBundle export**: produces `.textbundle` packages (each is a folder
//!    with `text.md` + `info.json`). Accepted when wrapped in a ZIP.
//! 3. **External Folders**: Ulysses writes live `.md` files to a user-chosen
//!    folder — a future Periodic path, not the import path handled here.
//!
//! # What a Markdown export contains
//!
//! When Ulysses exports sheets as Markdown, each sheet becomes a `.md` file
//! whose name is the sheet title. Groups become folders. No frontmatter or
//! YAML metadata is added — the content is verbatim Markdown. Modification
//! timestamps are carried in ZIP entry last-modified fields.
//!
//! A TextBundle (`.textbundle/`) within the ZIP is a directory containing:
//! - `text.md` — the sheet body
//! - `info.json` — `{ "version": 2, "type": "net.daringfireball.markdown",
//!   "creatorIdentifier": "com.soulmen.ulysses3", "transient": false }`
//!   (no per-note timestamps — those rely on the ZIP entry mtime).
//!
//! # Vault layout
//!
//! - **Contract** `notes/ulysses/YYYY-MM.jsonl` — one [`Note`] per sheet,
//!   partitioned by the local month of `created` (falls back to import
//!   time when no timestamp is available), deduped by `id` (the
//!   slash-normalised path stem within the export, stable across re-imports).
//! - **Raw** `notes/ulysses/raw/YYYY-MM.jsonl` — full-fidelity record
//!   (source path, body, mtime, group, format).
//!
//! # Re-import / dedupe
//!
//! The dedupe key is the entry path stem relative to the ZIP root (e.g.
//! `Work/Project Alpha` → `Work/Project Alpha`). Re-importing a newer export
//! upserts in place; it never duplicates.
//!
//! # Live XML library
//!
//! The native `.ulyz`/`.ulgroup` library format is proprietary XML. A parser
//! is feasible (community tools exist) but requires a real sample to validate
//! against. This path is **parked** (`parser_parked_needs_sample = true`);
//! the Markdown/TextBundle import is the primary and recommended path.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "ulysses";
const NOTES_DIR: &str = "notes/ulysses";
const RAW_DIR: &str = "notes/ulysses/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ulysses",
        name: "Ulysses",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your sheets from a Ulysses Markdown export ZIP. \
                      Re-runnable: re-importing a newer export updates sheets in place \
                      and never duplicates. For ongoing sync, enabling External Folders \
                      mode in Ulysses writes plain .md files that can be re-imported \
                      at any time.",
        domain: "notes",
        vault_path: "notes/ulysses/",
        toggleable: false,
        setup: &[
            "In Ulysses, open the sheet list for the group(s) you want to export.",
            "Choose File → Export All → Markdown (or TextBundle) to export your sheets.",
            "If the export produces a folder, compress it to a ZIP first.",
            "Drop the ZIP file into the import box.",
        ],
        caveats: "Plain Markdown exports carry no per-note creation timestamp; \
                  the import month is used as the partition key on first import. \
                  The native .ulyz library format requires Full Disk Access and \
                  a real sample for parser validation — use Markdown export instead.",
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
// Timestamp helpers

/// Try to extract a local RFC3339 timestamp from a ZIP entry's last-modified
/// time. `zip::DateTime` provides year/month/day/hour/minute/second fields.
/// Returns None if no mtime is available or it is invalid (before 1980).
fn zip_mtime_to_rfc3339(dt: Option<zip::DateTime>) -> Option<String> {
    let dt = dt?;
    // zip::DateTime year() returns the 4-digit year.
    if dt.year() < 1980 {
        return None;
    }
    let naive = NaiveDateTime::new(
        chrono::NaiveDate::from_ymd_opt(dt.year() as i32, dt.month() as u32, dt.day() as u32)?,
        chrono::NaiveTime::from_hms_opt(dt.hour() as u32, dt.minute() as u32, dt.second() as u32)?,
    );
    // ZIP mtimes are local time (no timezone info).
    let local: DateTime<Local> = Local.from_local_datetime(&naive).single()?;
    Some(local.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Body helpers

/// Extract a title from the first `# H1` line in a Markdown body.
/// Returns `None` if there is no H1 heading or the body is empty.
fn title_from_markdown(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        // Stop at the first non-empty line: if it's not an H1, don't search deeper.
        if !trimmed.is_empty() {
            break;
        }
    }
    None
}

/// Convert a ZIP-internal path to a stable sheet ID and a folder string.
///
/// The ZIP path looks like `GroupName/SubGroup/Sheet Title.md` or
/// `Sheet Title.md` for root-level sheets. We strip the file extension and use
/// the full slash-normalised path-without-extension as the id. The parent
/// directory (if any) becomes the `folder`.
///
/// For `.textbundle` packages: the outer bundle name is used (e.g.
/// `GroupName/Sheet Title.textbundle/text.md` → id = `GroupName/Sheet Title`,
/// folder = `GroupName`).
fn path_to_id_and_folder(zip_path: &str) -> (String, String) {
    // Normalise backslashes (Windows ZIPs).
    let normalised = zip_path.replace('\\', "/");

    // Check whether this path is inside a .textbundle package.
    let id_path: String = if let Some(bundle_end) = normalised.rfind(".textbundle/") {
        // Everything up to but not including ".textbundle/...".
        normalised[..bundle_end].to_string()
    } else if let Some(stripped) = normalised.strip_suffix(".md") {
        stripped.to_string()
    } else if let Some(stripped) = normalised.strip_suffix(".markdown") {
        stripped.to_string()
    } else {
        normalised.clone()
    };

    // Folder: the parent directory component of the id_path.
    let folder = if let Some(pos) = id_path.rfind('/') {
        id_path[..pos].to_string()
    } else {
        String::new()
    };

    (id_path, folder)
}

// ---------------------------------------------------------------------------
// Core import logic (extracted for testability)

/// Parse all `.md` and `.textbundle/text.md` entries from an open ZIP archive,
/// returning (contract notes, raw rows). Does not write anything.
pub(crate) fn parse_zip<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> (Vec<Note>, Vec<Value>) {
    let mut contract: Vec<Note> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();

    // Track which textbundle stems we have already processed so we don't emit
    // a duplicate row if the ZIP also has the plain `.md` sibling.
    let mut seen_ids: HashSet<String> = HashSet::new();

    // Collect the set of textbundle stems (paths like "Group/Title") so we can
    // skip the plain .md if a .textbundle is present for the same logical sheet.
    let bundle_stems: HashSet<String> = (0..archive.len())
        .filter_map(|i| archive.by_index_raw(i).ok().map(|e| e.name().to_string()))
        .filter(|n| n.contains(".textbundle/"))
        .map(|n| {
            let (stem, _) = path_to_id_and_folder(&n);
            stem
        })
        .collect();

    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.name().to_string();

        // Decide what kind of entry this is.
        let is_textbundle_body = name.contains(".textbundle/")
            && (name.ends_with("/text.md") || name.ends_with("/text.markdown"));
        let is_plain_md = !name.contains(".textbundle/")
            && (name.ends_with(".md") || name.ends_with(".markdown"));

        if !is_plain_md && !is_textbundle_body {
            continue;
        }

        let (id, folder) = path_to_id_and_folder(&name);

        // If a .textbundle exists for this stem, skip the plain .md sibling.
        if is_plain_md && bundle_stems.contains(&id) {
            continue;
        }

        // Dedupe within this ZIP (same logical sheet appearing twice).
        if !seen_ids.insert(id.clone()) {
            continue;
        }

        // Read the body.
        let mut body = String::new();
        if entry.read_to_string(&mut body).is_err() {
            // Non-UTF-8 content — skip this entry.
            continue;
        }

        // Timestamps from the ZIP entry mtime (returns Option<zip::DateTime>).
        let modified = zip_mtime_to_rfc3339(entry.last_modified());
        // No creation timestamp in a plain Markdown export; fall back to mtime.
        let created = modified.clone();

        // Title: H1 from body, else the filename stem (last component of id).
        let stem_title = id
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let title = title_from_markdown(&body)
            .unwrap_or_else(|| stem_title.clone());

        // --- Contract note ---
        let mut note = Note::new(SOURCE, &id);
        note.title = title.clone();
        note.body = body.clone();
        if let Some(ref c) = created {
            note.created = c.clone();
        }
        if let Some(ref m) = modified {
            note.modified = m.clone();
        }
        if !folder.is_empty() {
            note.folder = folder.clone();
        }
        // Extra: format marker (textbundle vs plain markdown).
        let mut extra = Map::new();
        if is_textbundle_body {
            extra.insert("format".into(), Value::String("textbundle".into()));
        } else {
            extra.insert("format".into(), Value::String("markdown".into()));
        }
        note.extra = extra;

        // --- Raw row (full fidelity) ---
        let mut raw = Map::new();
        raw.insert("source".into(), Value::String(SOURCE.into()));
        raw.insert("id".into(), Value::String(id.clone()));
        raw.insert("zip_path".into(), Value::String(name));
        raw.insert("title".into(), Value::String(title));
        raw.insert("body".into(), Value::String(body));
        if !folder.is_empty() {
            raw.insert("folder".into(), Value::String(folder));
        }
        if is_textbundle_body {
            raw.insert("format".into(), Value::String("textbundle".into()));
        } else {
            raw.insert("format".into(), Value::String("markdown".into()));
        }
        if let Some(m) = &modified {
            raw.insert("modified".into(), Value::String(m.clone()));
        }
        if let Some(c) = &created {
            raw.insert("created".into(), Value::String(c.clone()));
            // Immutable partition key for the raw layer.
            raw.insert("_created".into(), Value::String(c.clone()));
        }
        raw_rows.push(Value::Object(raw));
        contract.push(note);
    }

    (contract, raw_rows)
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", path.display()))?;

    let (contract, raw_rows) = parse_zip(&mut archive);

    let imported = contract.len() as u64;

    if !contract.is_empty() {
        vault.upsert_ulysses_notes(&contract)?;
        vault.upsert_ulysses_raw(&raw_rows)?;
    }

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} sheet{} imported", if imported == 1 { "" } else { "s" }),
        counts: [("imported", imported)].into(),
    })
}

// ---------------------------------------------------------------------------
// Vault helpers

impl Vault {
    /// Upsert contract notes into `notes/ulysses/YYYY-MM.jsonl`,
    /// partitioned by the month of `created` (falls back to `modified` when
    /// `created` is absent), deduped by `id`.
    pub fn upsert_ulysses_notes(&self, notes: &[Note]) -> Result<()> {
        use std::collections::HashMap;
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            let ts = if !n.created.is_empty() {
                &n.created
            } else if !n.modified.is_empty() {
                &n.modified
            } else {
                // No timestamp at all — skip from the contract layer.
                // The raw layer still persists it via upsert_ulysses_raw.
                continue;
            };
            if let Some(key) = Partition::Month.key(ts) {
                by_month.entry(key.to_string()).or_default().push(n);
            }
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|n| n.id.as_str()).collect();
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

    /// Upsert raw rows into `notes/ulysses/raw/YYYY-MM.jsonl`,
    /// partitioned by `_created` (immutable), deduped by `id`.
    pub fn upsert_ulysses_raw(&self, rows: &[Value]) -> Result<()> {
        use std::collections::HashMap;
        fn month_of(v: &Value) -> &str {
            v.get("_created")
                .or_else(|| v.get("modified"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
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
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|v| id_of(v)).collect();
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
    use crate::vault::Vault;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn temp_vault(name: &str) -> (Vault, std::path::PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("trove-ulysses-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault_dir = dir.join("vault");
        let v = Vault::open_or_create(vault_dir).unwrap();
        (v, dir)
    }

    /// Build a ZIP from a list of `(path_in_zip, content)` entries, all with
    /// a fixed mtime of 2026-03-15T10:00:00 local.
    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default().last_modified_time(
                zip::DateTime::from_date_and_time(2026, 3, 15, 10, 0, 0).unwrap(),
            );
            for (path, content) in entries {
                w.start_file(*path, opts).unwrap();
                w.write_all(content.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    // -----------------------------------------------------------------------
    // Unit: path_to_id_and_folder

    #[test]
    fn path_to_id_root_level() {
        let (id, folder) = path_to_id_and_folder("My Sheet.md");
        assert_eq!(id, "My Sheet");
        assert_eq!(folder, "");
    }

    #[test]
    fn path_to_id_nested_group() {
        let (id, folder) = path_to_id_and_folder("Work/Project Alpha/Meeting Notes.md");
        assert_eq!(id, "Work/Project Alpha/Meeting Notes");
        assert_eq!(folder, "Work/Project Alpha");
    }

    #[test]
    fn path_to_id_textbundle() {
        let (id, folder) = path_to_id_and_folder("Work/Sheet.textbundle/text.md");
        assert_eq!(id, "Work/Sheet");
        assert_eq!(folder, "Work");
    }

    #[test]
    fn path_to_id_root_textbundle() {
        let (id, folder) = path_to_id_and_folder("My Note.textbundle/text.md");
        assert_eq!(id, "My Note");
        assert_eq!(folder, "");
    }

    // -----------------------------------------------------------------------
    // Unit: title_from_markdown

    #[test]
    fn title_from_h1() {
        assert_eq!(
            title_from_markdown("# My Title\n\nSome body text."),
            Some("My Title".into())
        );
    }

    #[test]
    fn title_from_body_no_h1() {
        assert_eq!(
            title_from_markdown("No heading here\n\nJust body."),
            None
        );
    }

    #[test]
    fn title_from_empty_body() {
        assert_eq!(title_from_markdown(""), None);
    }

    // -----------------------------------------------------------------------
    // Integration: parse_zip

    /// Fixture: exported Markdown sheet (group-level, with H1 heading).
    /// Shape confirmed from TextBundle spec + community Ulysses export
    /// descriptions: plain .md, no frontmatter, body = verbatim Markdown.
    const MARKDOWN_BODY: &str =
        "# Garden planting plan\n\n- Tomatoes in the south bed\n- Basil nearby\n";
    const TEXTBUNDLE_BODY: &str =
        "# Weekend tasks\n\nFinish the report\nCall dentist\n";

    #[test]
    fn parses_plain_markdown_entries() {
        let bytes = make_zip(&[("Work/Garden planting plan.md", MARKDOWN_BODY)]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let (notes, raws) = parse_zip(&mut archive);

        assert_eq!(notes.len(), 1);
        let n = &notes[0];
        assert_eq!(n.source, "ulysses");
        assert_eq!(n.id, "Work/Garden planting plan");
        assert_eq!(n.title, "Garden planting plan");
        assert!(n.body.contains("Tomatoes"));
        assert_eq!(n.folder, "Work");
        // mtime carried from ZIP entry (2026-03-15).
        assert!(n.created.starts_with("2026-03-15"), "created derived from zip mtime");
        assert_eq!(n.extra.get("format").and_then(|v| v.as_str()), Some("markdown"));

        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0]["id"], "Work/Garden planting plan");
        assert_eq!(raws[0]["format"], "markdown");
        assert!(raws[0]["body"].as_str().unwrap().contains("Tomatoes"));
    }

    #[test]
    fn parses_textbundle_entries() {
        let bytes = make_zip(&[
            ("Personal/Weekend tasks.textbundle/text.md", TEXTBUNDLE_BODY),
            // info.json inside the bundle — must be ignored.
            (
                "Personal/Weekend tasks.textbundle/info.json",
                r#"{"version":2,"type":"net.daringfireball.markdown","creatorIdentifier":"com.soulmen.ulysses3"}"#,
            ),
        ]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let (notes, raws) = parse_zip(&mut archive);

        assert_eq!(notes.len(), 1, "only text.md row; info.json skipped");
        let n = &notes[0];
        assert_eq!(n.id, "Personal/Weekend tasks");
        assert_eq!(n.title, "Weekend tasks");
        assert_eq!(n.folder, "Personal");
        assert_eq!(
            n.extra.get("format").and_then(|v| v.as_str()),
            Some("textbundle")
        );
        assert_eq!(raws[0]["format"], "textbundle");
    }

    #[test]
    fn textbundle_preferred_over_sibling_md() {
        // If both Sheet.md and Sheet.textbundle/text.md exist for the same
        // logical sheet, the textbundle wins and only one row is emitted.
        let bytes = make_zip(&[
            ("Sheet.textbundle/text.md", "# TB Version\n\nfrom bundle"),
            ("Sheet.md", "# MD Version\n\nfrom plain"),
        ]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let (notes, _) = parse_zip(&mut archive);

        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].extra.get("format").and_then(|v| v.as_str()),
            Some("textbundle"),
            "textbundle variant wins over plain .md for same stem"
        );
    }

    #[test]
    fn root_level_sheet_has_no_folder() {
        let bytes = make_zip(&[("Top Level Note.md", "# Top Level Note\n\nbody")]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let (notes, raws) = parse_zip(&mut archive);

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].folder, "", "root-level sheet has no folder");
        assert!(
            raws[0].get("folder").is_none(),
            "folder absent from raw for root-level sheet"
        );
    }

    #[test]
    fn body_without_h1_uses_filename_stem_as_title() {
        let bytes = make_zip(&[("My Thoughts.md", "Some notes without a heading.\n")]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let (notes, _) = parse_zip(&mut archive);

        assert_eq!(
            notes[0].title, "My Thoughts",
            "filename stem used when body has no H1"
        );
    }

    #[test]
    fn non_md_entries_are_skipped() {
        let bytes = make_zip(&[
            ("README.txt", "not a sheet"),
            ("image.png", "PNG"),
            ("Sheet.md", "# Real Sheet\n\nContent."),
        ]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let (notes, _) = parse_zip(&mut archive);

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, "Sheet");
    }

    // -----------------------------------------------------------------------
    // Integration: vault upsert + dedupe

    #[test]
    fn upsert_writes_and_dedupes_by_id() {
        let (v, _tmp) = temp_vault("upsert");

        // First import: two sheets.
        let bytes = make_zip(&[
            ("Work/Plan.md", "# Project Plan\n\nPhase 1."),
            ("Personal/Diary.md", "# Diary\n\nToday was good."),
        ]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let (notes, raws) = parse_zip(&mut archive);
        v.upsert_ulysses_notes(&notes).unwrap();
        v.upsert_ulysses_raw(&raws).unwrap();

        // Re-import the same ZIP — must not duplicate.
        let bytes2 = make_zip(&[("Work/Plan.md", "# Project Plan\n\nPhase 1.")]);
        let mut archive2 = zip::ZipArchive::new(std::io::Cursor::new(bytes2)).unwrap();
        let (notes2, raws2) = parse_zip(&mut archive2);
        v.upsert_ulysses_notes(&notes2).unwrap();
        v.upsert_ulysses_raw(&raws2).unwrap();

        // Total across all month files must be exactly 2 unique notes.
        let dir = v.root().join(NOTES_DIR);
        let total: usize = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .map(|e| {
                let month = e
                    .path()
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                v.stream(NOTES_DIR, Partition::Month).read::<Note>(&month).unwrap().len()
            })
            .sum();
        assert_eq!(total, 2, "two unique sheets, no duplicates after re-import");
    }

    #[test]
    fn run_import_returns_count() {
        let (v, tmp) = temp_vault("run");
        let bytes = make_zip(&[
            ("Sheet A.md", "# Sheet A\n\nbody A"),
            ("Sheet B.md", "# Sheet B\n\nbody B"),
        ]);
        let zip_path = tmp.join("export.zip");
        std::fs::write(&zip_path, &bytes).unwrap();

        let mut progress_called = false;
        let outcome = run_import(&v, &zip_path, &Default::default(), &mut |_| {
            progress_called = true;
        })
        .unwrap();

        assert_eq!(outcome.counts["imported"], 2);
        assert!(outcome.headline.contains("2 sheets"));
        assert!(progress_called);
    }

    #[test]
    fn serde_back_compat_old_note_lines_still_deserialize() {
        // Old lines with only `source` and `id` must still deserialize —
        // additive evolution (matches the notes contract backcompat spec).
        let v = temp_vault("compat").0;
        std::fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        std::fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-03.jsonl")),
            "{\"source\":\"ulysses\",\"id\":\"old/sheet\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "old/sheet");
        assert_eq!(rows[0].source, "ulysses");
    }
}
