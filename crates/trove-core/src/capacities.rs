//! Capacities — import of automated local ZIP exports from the Capacities
//! note-taking / personal-knowledge app (no cloud round-trip required).
//!
//! Capacities stores notes as typed objects ("pages", "daily notes", "tags",
//! collections). As of May 2025 it can run scheduled local export ZIPs straight
//! to disk with no cloud round-trip — making it a clean watch-folder import.
//! Free tier supports the export.
//!
//! **Export format (documented):** The ZIP contains Markdown files whose YAML
//! frontmatter carries all object properties ("All your properties are
//! transformed into front matter values"). Collections are also exported as CSV
//! files. Media assets land in separate sub-folders. File and folder names are
//! human-readable. Links are converted to local references.
//!
//! **Parser status — PARKED (Needs-sample):** The exact YAML frontmatter
//! *field names* for the stable object id, creation timestamp, modification
//! timestamp, and object type are **not documented** by Capacities and no real
//! export ZIP has been obtained for this integration. Writing a parser against
//! an assumed field shape would risk the raindrop-_id / fathom-transcript trap
//! (a green test against a wrong fixture is false confidence). The raw layer
//! (full fidelity) is written unconditionally so all content is preserved from
//! day one; the contract layer parser is parked until a real export ZIP lands.
//! See `docs/integrations/capacities.md` for the `Needs-sample` flag.
//!
//! **What ships today:**
//!   - `Behavior::Import` (accepts `zip`)
//!   - Raw layer: `notes/capacities/raw/` — each `.md` entry in the export ZIP
//!     as a JSON object `{source, path, content, zip_mtime}`, partitioned by
//!     the ZIP entry's last-modified date (or import date when the ZIP carries
//!     no mtime). Full fidelity.
//!   - Contract layer: PARKED — no rows written until field names are confirmed
//!     against a real export. The contract writer is structurally complete and
//!     will be unparked by replacing the `parse_object` stub below.
//!   - Re-import dedup: `path` (ZIP-relative) as the best-effort key. Because
//!     Capacities file names are title-derived, the path changes if a note is
//!     renamed — an old-path row is then orphaned. A stable object `id` (from
//!     frontmatter) will replace `path` as the dedup key when the parser is
//!     unparked. Cross-month dedup is global: before writing, all existing raw
//!     partitions are scanned and any row whose `path` matches an incoming path
//!     is removed, so a re-import near a month boundary cannot produce
//!     duplicates even when the ZIP entry mtime (and therefore `_created`)
//!     differs between runs.
//!
//! **To unpark:** obtain a real Capacities export ZIP, `unzip -l` it to verify
//! the directory layout, open one `.md` file to read the YAML frontmatter field
//! names for object id, created date, modified date, and object type, then fill
//! in `parse_object` below. Remove the `Needs-sample` flag from the brief and
//! this doc-comment, set `parser_parked_needs_sample=false` in the build output.
//!
//! Brief: `docs/integrations/capacities.md`.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, TimeZone as _};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "capacities";
const NOTES_DIR: &str = "notes/capacities";
const RAW_DIR: &str = "notes/capacities/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "capacities",
        name: "Capacities",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Capacities notes and objects from a local ZIP \
                      export. Schedule automatic exports in Capacities (Settings → \
                      Export; up to 5 schedules, free tier included) and drop the \
                      latest ZIP here to sync your space into Trove. No credentials \
                      required — the export engine runs locally on your Mac.",
        domain: "notes",
        vault_path: "notes/capacities/",
        toggleable: false,
        setup: &[
            "In Capacities: Settings → Export → add a scheduled export to a \
             folder you can reach (e.g. ~/Downloads). Daily or weekly cadence is \
             sufficient.",
            "Drop the most-recent exported ZIP here. Re-import at any time to \
             update notes in place.",
        ],
        caveats: "Automatic local exports require the Capacities app to be running \
                  on a schedule; free tier is sufficient. The contract-layer parser \
                  is pending a real export sample — raw content is preserved in full \
                  from day one.",
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
// ZIP extraction

/// One Markdown entry extracted from the export ZIP.
struct ExportEntry {
    /// ZIP-relative path, e.g. `Space Name/Notes/My note.md`. Used as the
    /// best-effort dedupe key across re-imports (title-derived; changes on
    /// rename; replaced by the frontmatter object `id` when the parser is
    /// unparked).
    path: String,
    /// File content as UTF-8 (lossy: non-UTF-8 bytes are replaced with U+FFFD
    /// so we never panic on unusual encodings while still preserving intent).
    content: String,
    /// ZIP entry last-modified timestamp as RFC3339 local time, if available.
    /// DOS-time in ZIP has 2-second resolution and no timezone — stored as local.
    zip_mtime: Option<String>,
}

/// Read all `.md` entries from the export ZIP. Returns an empty vec when the
/// path isn't a valid ZIP — callers treat that as an error via the anyhow
/// context.
///
/// Only `.md` is accepted: Capacities documents note exports exclusively as
/// Markdown; collections are `.csv` (handled separately when the parser is
/// unparked). Accepting `.txt` would pull in stray non-note text files with
/// no evidence that Capacities produces any.
fn read_export_entries(path: &Path) -> Result<Vec<ExportEntry>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP {}", path.display()))?;

    let mut out = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("reading ZIP entry #{i}"))?;
        let name = entry.name().to_string();

        // Skip directories and non-Markdown files. Capacities exports notes
        // as `.md`; CSV collections and media are excluded until the parser
        // is unparked.
        let lower = name.to_ascii_lowercase();
        if entry.is_dir() || !lower.ends_with(".md") {
            continue;
        }
        // Skip macOS __MACOSX metadata entries.
        if name.starts_with("__MACOSX/") || name.contains("/__MACOSX/") {
            continue;
        }

        // Read content.
        let mut raw_bytes = Vec::new();
        entry.read_to_end(&mut raw_bytes).with_context(|| {
            format!("reading content of ZIP entry {name}")
        })?;
        let content = String::from_utf8_lossy(&raw_bytes).into_owned();

        // ZIP last-modified time (DOS-time, 2-second resolution, no timezone).
        // `entry.last_modified()` returns `Option<zip::DateTime>` in zip 8.x.
        // The `chrono` feature is not enabled in our dep, so we use the
        // year/month/day/hour/minute/second accessors directly (same pattern
        // as logseq.rs).
        let zip_mtime = entry.last_modified().and_then(|dt| {
            chrono::NaiveDate::from_ymd_opt(
                dt.year().into(),
                dt.month().into(),
                dt.day().into(),
            )
            .and_then(|d| {
                d.and_hms_opt(dt.hour().into(), dt.minute().into(), dt.second().into())
            })
            .map(|ndt| {
                // Treat as local time (DOS timestamps are local, unzoned).
                Local
                    .from_local_datetime(&ndt)
                    .earliest()
                    .map(|ldt| ldt.to_rfc3339())
                    .unwrap_or_else(|| ndt.and_utc().to_rfc3339())
            })
        });

        out.push(ExportEntry { path: name, content, zip_mtime });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Contract-layer parser (PARKED — Needs-sample)
//
// Capacities exports Markdown with YAML frontmatter. The properties of each
// object (created date, modified date, object type, tags, etc.) become
// frontmatter values. To write the contract layer we need the exact field
// names for:
//   - The stable object id (e.g. `id:`, `uid:`, `capacities_id:`, or similar)
//   - The creation timestamp (e.g. `created:`, `createdAt:`, `date:`)
//   - The modification timestamp (e.g. `modified:`, `updatedAt:`)
//   - The object type (e.g. `type:`, `structureId:`)
//   - Tags (e.g. `tags:`, a list)
//
// These are NOT documented by Capacities and no real export sample is
// available. The function below is a STUB that returns None for every entry
// until a real sample confirms the field names. Replace the stub body once a
// real export ZIP is obtained.
//
// IMPORTANT: Do not guess field names. Green tests against an assumed shape
// are false confidence (the raindrop _id zero-collection bug precedent).

/// Attempt to extract a contract [`Note`] from a raw Markdown entry.
/// Returns `None` (stub — Needs-sample): the real frontmatter field names are
/// not yet confirmed. Unpark by filling in the frontmatter key names from a
/// real Capacities export and replacing this stub body.
fn parse_object(_entry: &ExportEntry) -> Option<Note> {
    // PARKED — replace with real implementation once a sample export is obtained.
    // Expected shape (unconfirmed):
    //   ---
    //   id: <uuid>              ← unknown key name
    //   created: <ISO-8601>     ← unknown key name
    //   modified: <ISO-8601>    ← unknown key name
    //   type: <structureId>     ← unknown key name
    //   tags: [...]             ← unknown key name
    //   ---
    //   <markdown body>
    None
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let entries = read_export_entries(path)?;
    let total = entries.len() as u64;
    if total == 0 {
        return Ok(ImportOutcome {
            headline: "No Markdown files found in the ZIP — is this a Capacities export?".into(),
            counts: [("imported", 0u64), ("updated", 0u64), ("skipped", 0u64)].into(),
        });
    }

    // Dedupe: path (ZIP-relative, best-effort — title-derived, changes on
    // rename; replaced by frontmatter object id when the parser is unparked).
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in raw_stream.partitions()? {
        for obj in raw_stream.read::<Value>(&key)? {
            if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
                seen.insert(p.to_string());
            }
        }
    }

    let import_ts = Local::now().to_rfc3339();
    let (mut imported, mut updated, skipped) = (0u64, 0u64, 0u64);
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut contract_notes: Vec<Note> = Vec::new();

    for (idx, entry) in entries.iter().enumerate() {
        let is_update = seen.contains(&entry.path);
        seen.insert(entry.path.clone());

        // Partition key: ZIP entry mtime, or import timestamp as fallback.
        let partition_ts = entry.zip_mtime.clone().unwrap_or_else(|| import_ts.clone());

        // Raw layer: unconditional full-fidelity row. Every character of the
        // ZIP entry content is preserved here regardless of whether the contract
        // parser can extract structured fields from the frontmatter.
        let mut raw_obj = Map::new();
        raw_obj.insert("source".into(), Value::from(SOURCE));
        raw_obj.insert("path".into(), Value::from(entry.path.clone()));
        raw_obj.insert("content".into(), Value::from(entry.content.clone()));
        if let Some(mtime) = &entry.zip_mtime {
            raw_obj.insert("zip_mtime".into(), Value::from(mtime.clone()));
        }
        // Partition anchor stored on the raw row so the global dedup can find
        // it.  This value equals the ZIP entry mtime (or import-time fallback)
        // and can shift between re-imports — the global cross-month dedup in
        // `upsert_capacities_raw` handles that correctly.
        raw_obj.insert("_created".into(), Value::from(partition_ts.clone()));
        raw_rows.push(Value::Object(raw_obj));

        // Contract layer: PARKED (Needs-sample). parse_object always returns
        // None until the frontmatter field names are confirmed against a real
        // export. When it returns Some, the note is upserted into the contract
        // layer alongside the raw row.
        if let Some(mut note) = parse_object(&entry) {
            // Ensure the partition ts is set so the note lands in a month file.
            if note.created.is_empty() {
                note.created = partition_ts.clone();
            }
            contract_notes.push(note);
        }

        if is_update {
            updated += 1;
        } else {
            imported += 1;
        }

        if (idx as u64 + 1) % 100 == 0 {
            let pct = (idx as f32 + 1.0) / total.max(1) as f32 * 90.0;
            progress(ImportProgress { records: imported + updated, percent: pct });
        }

        let _ = skipped; // used below
    }

    // Persist the raw layer (always).
    if !raw_rows.is_empty() {
        vault.upsert_capacities_raw(&raw_rows)?;
    }
    // Persist the contract layer when the parser is unparked.
    if !contract_notes.is_empty() {
        vault.upsert_capacities_notes(&contract_notes)?;
    }

    progress(ImportProgress { records: imported + updated, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{} notes imported (raw), {} updated — contract layer parked pending sample",
            imported, updated
        ),
        counts: [("imported", imported), ("updated", updated), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// Vault impl — upsert helpers (same snapshot+upsert pattern as logseq.rs)

impl Vault {
    /// Upsert contract notes into `notes/capacities/YYYY-MM.jsonl`,
    /// partitioned by the month of `created`, deduped by `id`.
    pub fn upsert_capacities_notes(&self, notes: &[Note]) -> Result<()> {
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

    /// Upsert raw rows into `notes/capacities/raw/YYYY-MM.jsonl`, partitioned
    /// by the month of `_created`, deduped by `path` **globally** across all
    /// month partitions.
    ///
    /// A ZIP entry mtime is mutable (re-export or import near a month boundary
    /// can shift it), so `_created` — which is derived from that mtime — can
    /// change between runs.  Per-month dedup is not sufficient: the same `path`
    /// would end up in two different month files.  Instead, before writing any
    /// incoming row we strip its `path` from *every* existing partition, then
    /// write it into the target month.  This guarantees at most one row per
    /// `path` across the entire raw layer.
    pub fn upsert_capacities_raw(&self, rows: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn key_of(v: &Value) -> &str {
            v.get("path").and_then(|p| p.as_str()).unwrap_or("")
        }

        // Collect the set of paths that are being written this run.
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in rows {
            let Some(key) = Partition::Month.key(month_of(v)) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(v);
        }
        if by_month.is_empty() {
            return Ok(());
        }

        let incoming_paths: HashSet<&str> =
            rows.iter().map(|v| key_of(v)).collect();

        let stream = self.stream(RAW_DIR, Partition::Month);

        // Pass 1 — strip incoming paths from every EXISTING partition that is
        // NOT a target month.  This removes cross-month duplicates caused by a
        // shifting `_created`.
        let existing_partitions = stream.partitions()?;
        for existing_month in &existing_partitions {
            if by_month.contains_key(existing_month) {
                // Target months are handled in pass 2.
                continue;
            }
            let survivors: Vec<Value> = stream
                .read::<Value>(existing_month)?
                .into_iter()
                .filter(|v| !incoming_paths.contains(key_of(v)))
                .collect();
            let rel = format!("{RAW_DIR}/{existing_month}.jsonl");
            self.write_snapshot(&rel, &survivors)?;
        }

        // Pass 2 — merge into target months (remove stale same-month rows,
        // append incoming rows).
        for (month, incoming) in by_month {
            let incoming_keys: HashSet<&str> =
                incoming.iter().map(|v| key_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|existing| !incoming_keys.contains(key_of(existing)))
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
            .join(format!("trove-capacities-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a minimal Capacities-like export ZIP.
    fn make_zip(v: &Vault, name: &str, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let zip_path = v.root().join(format!("{name}.zip"));
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .last_modified_time(zip::DateTime::from_date_and_time(2026, 6, 10, 12, 0, 0).unwrap());
        for (path, content) in entries {
            w.start_file(*path, opts).unwrap();
            w.write_all(content.as_bytes()).unwrap();
        }
        w.finish().unwrap();
        zip_path
    }

    #[test]
    fn imports_markdown_files_into_raw_layer() {
        let v = temp_vault("basic");
        let zip = make_zip(&v, "export", &[
            ("My Space/Notes/Garden plan.md",
             "---\n# (frontmatter TBD)\n---\n\n# Garden plan\n\nTomatoes in the south bed.\n"),
            ("My Space/Daily Notes/2026-06-10.md",
             "---\n# (frontmatter TBD)\n---\n\nLong run along the coast.\n"),
        ]);

        let out = (IMPORT.run)(&v, &zip, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert!(
            out.headline.contains("2 notes imported"),
            "unexpected headline: {}",
            out.headline
        );
        assert_eq!(out.counts.get("imported"), Some(&2));

        // Raw layer: two rows in 2026-06 (the ZIP entry mtime month).
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let rows: Vec<Value> = raw_stream.read("2026-06").unwrap();
        assert_eq!(rows.len(), 2, "both files in raw layer");
        let paths: Vec<&str> = rows.iter()
            .filter_map(|r| r.get("path").and_then(|v| v.as_str()))
            .collect();
        assert!(paths.contains(&"My Space/Notes/Garden plan.md"));
        assert!(paths.contains(&"My Space/Daily Notes/2026-06-10.md"));
        // Full content preserved.
        let garden = rows.iter().find(|r| {
            r.get("path").and_then(|v| v.as_str())
                == Some("My Space/Notes/Garden plan.md")
        }).unwrap();
        assert!(
            garden["content"].as_str().unwrap().contains("Tomatoes"),
            "content is full-fidelity"
        );
        // Source tag.
        assert_eq!(garden["source"].as_str(), Some("capacities"));
    }

    #[test]
    fn reimport_dedup_by_path() {
        let v = temp_vault("dedup");
        let entries = &[
            ("Space/Notes/Note A.md", "---\n---\n\nBody A.\n"),
            ("Space/Notes/Note B.md", "---\n---\n\nBody B.\n"),
        ];
        let zip = make_zip(&v, "export1", entries);
        let out1 = (IMPORT.run)(&v, &zip, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out1.counts.get("imported"), Some(&2));
        assert_eq!(out1.counts.get("updated"), Some(&0));

        // Re-import the same ZIP: both paths already seen → updated, not imported.
        let zip2 = make_zip(&v, "export2", entries);
        let out2 = (IMPORT.run)(&v, &zip2, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out2.counts.get("imported"), Some(&0), "no new rows");
        assert_eq!(out2.counts.get("updated"), Some(&2), "both updated in place");

        // Raw layer must have exactly 2 rows (no duplicates).
        let rows: Vec<Value> = v.stream(RAW_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 2, "dedup: no duplicate rows");
    }

    #[test]
    fn skips_non_markdown_and_macosx_entries() {
        let v = temp_vault("skip");
        let zip_path = v.root().join("export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .last_modified_time(zip::DateTime::from_date_and_time(2026, 6, 10, 12, 0, 0).unwrap());
        w.start_file("Space/Notes/Real.md", opts).unwrap();
        w.write_all(b"# Real note\n").unwrap();
        w.start_file("Space/collections/Tags.csv", opts).unwrap();
        w.write_all(b"name,count\nwork,5\n").unwrap();
        w.start_file("__MACOSX/Space/._Notes", opts).unwrap();
        w.write_all(b"\x00\x00\x00\x00").unwrap();
        w.start_file("Space/media/image.png", opts).unwrap();
        w.write_all(b"\x89PNG\r\n").unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&1), "only .md imported");

        let rows: Vec<Value> = v.stream(RAW_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["path"].as_str(), Some("Space/Notes/Real.md"));
    }

    #[test]
    fn empty_zip_returns_informative_headline() {
        let v = temp_vault("empty");
        let zip_path = v.root().join("empty.zip");
        // A valid ZIP with no files.
        zip::ZipWriter::new(fs::File::create(&zip_path).unwrap()).finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert!(out.headline.contains("No Markdown files"), "headline: {}", out.headline);
        assert_eq!(out.counts.get("imported"), Some(&0));
    }

    #[test]
    fn contract_layer_parked_pending_sample() {
        // parse_object must return None for every entry until a real sample is
        // obtained and the stub is filled in — this test protects against
        // accidental fabrication.
        let entry = ExportEntry {
            path: "Space/Notes/Test.md".into(),
            content: "---\nid: some-uuid\ncreated: 2026-06-10\n---\n\nBody.\n".into(),
            zip_mtime: Some("2026-06-10T12:00:00+00:00".into()),
        };
        assert!(
            parse_object(&entry).is_none(),
            "parse_object must remain None (parked) until Needs-sample is resolved"
        );
    }

    /// Prove the cross-month dedup fix (major defect: a path with a shifting
    /// `_created` — due to mutable ZIP entry mtime — must not accumulate in
    /// two month files simultaneously).
    #[test]
    fn cross_month_dedup_no_duplicate_on_shifted_created() {
        let v = temp_vault("crossmonth");

        // First import: note lands in 2026-05 (mtime set to 2026-05-20).
        let zip_path = v.root().join("may.zip");
        {
            let mut w =
                zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default()
                .last_modified_time(
                    zip::DateTime::from_date_and_time(2026, 5, 20, 12, 0, 0)
                        .unwrap(),
                );
            w.start_file("Space/Notes/N.md", opts).unwrap();
            w.write_all(b"# Note\nVersion 1\n").unwrap();
            w.finish().unwrap();
        }
        let out1 =
            (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out1.counts.get("imported"), Some(&1));

        // Verify the row lives in 2026-05.
        let may_rows: Vec<Value> =
            v.stream(RAW_DIR, Partition::Month).read("2026-05").unwrap();
        assert_eq!(may_rows.len(), 1, "first import: one row in 2026-05");

        // Second import: same path but ZIP mtime is now 2026-06 (simulates
        // the note being re-exported after an edit, or a fallback to import
        // time near a month boundary).
        let zip_path2 = v.root().join("jun.zip");
        {
            let mut w =
                zip::ZipWriter::new(std::fs::File::create(&zip_path2).unwrap());
            let opts = zip::write::SimpleFileOptions::default()
                .last_modified_time(
                    zip::DateTime::from_date_and_time(2026, 6, 3, 12, 0, 0)
                        .unwrap(),
                );
            w.start_file("Space/Notes/N.md", opts).unwrap();
            w.write_all(b"# Note\nVersion 2\n").unwrap();
            w.finish().unwrap();
        }
        let out2 =
            (IMPORT.run)(&v, &zip_path2, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out2.counts.get("updated"), Some(&1));

        // The old 2026-05 row MUST be gone (global cross-month dedup).
        let may_rows_after: Vec<Value> =
            v.stream(RAW_DIR, Partition::Month).read("2026-05").unwrap();
        assert_eq!(
            may_rows_after.len(),
            0,
            "old-month row must be removed after cross-month re-import"
        );

        // Exactly one row total across both months.
        let jun_rows: Vec<Value> =
            v.stream(RAW_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(jun_rows.len(), 1, "new-month row present");
        assert!(
            jun_rows[0]["content"]
                .as_str()
                .unwrap()
                .contains("Version 2"),
            "updated content present"
        );
    }

    #[test]
    fn last_data_reflects_imported_month() {
        let v = temp_vault("lastdata");
        let zip = make_zip(&v, "exp", &[
            ("Space/note.md", "# hello\n"),
        ]);
        (IMPORT.run)(&v, &zip, &BTreeMap::new(), &mut |_| {}).unwrap();

        // last_data reads the newest stem from notes/capacities/ — which is the
        // contract layer. Since the contract parser is parked it writes nothing,
        // so last_data is None. When unparked, this test should assert Some("2026-06").
        // For now, confirm it does not panic.
        let _ld = def_last_data(&v);
        // Raw layer should have a stem regardless.
        let newest_raw = crate::registry::newest_stem(&v.root().join(RAW_DIR));
        assert_eq!(newest_raw.as_deref(), Some("2026-06"), "raw layer has the stem");
    }

    #[test]
    fn serde_back_compat_old_contract_lines_still_deserialize() {
        // A Note written by a future unparked writer (only required fields) must
        // still deserialize — additive schema evolution.
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-06.jsonl")),
            "{\"source\":\"capacities\",\"id\":\"abc-123\"}\n\
             {\"source\":\"capacities\",\"id\":\"def-456\",\"title\":\"Garden\",\"created\":\"2026-06-10T09:00:00-07:00\"}\n",
        ).unwrap();
        let rows: Vec<Note> = v.stream(NOTES_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "abc-123");
        assert_eq!(rows[1].title, "Garden");
    }
}
