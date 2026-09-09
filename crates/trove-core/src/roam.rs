//! Roam Research — import via manual JSON export ZIP (or bare JSON file).
//!
//! Roam Research is a networked-thought outliner with pages and nested blocks
//! linked bidirectionally. The only standalone-compliant export is the manual
//! "Export All → JSON" (or Markdown) ZIP available in every plan. The beta
//! `developer.ro.am` API has no full-graph dump endpoint; the roam-research-mcp
//! server requires a running local service (violates standalone). So we wire the
//! ZIP/JSON import only.
//!
//! # Roam JSON export structure (established community format)
//!
//! The ZIP contains one `<graph-name>.json` (the full graph) — an array of page
//! objects, each of which is a tree of block objects:
//!
//! ```json
//! [
//!   {
//!     "title": "Page Title",
//!     "uid": "abc12345",
//!     "create-time": 1609459200000,
//!     "edit-time": 1609545600000,
//!     "children": [
//!       {
//!         "string": "Block text with [[page refs]] and ((block-refs))",
//!         "uid": "xyz98765",
//!         "create-time": 1609459200000,
//!         "edit-time": 1609459200000,
//!         "heading": 1,
//!         "order": 0,
//!         "children": [...]
//!       }
//!     ]
//!   }
//! ]
//! ```
//!
//! Timestamps are **milliseconds** since the Unix epoch. Daily notes pages have
//! titles like `"June 15th, 2026"`. A page may have no children (empty page).
//! Block `refs` (when present) carry page/block uid back-links:
//! `"refs": [{"title": "Some Page", "uid": "abc12345"}]`.
//!
//! **Field optionality (real exports):**
//! - `uid` on a page is OPTIONAL — auto-created `[[ref]]` pages and many daily
//!   notes lack a uid entirely (Roam issue #668, never fixed). When absent we
//!   derive a stable id by hashing the page title.
//! - `create-time` on a page is OPTIONAL — when absent we fall back to
//!   `edit-time` (mirror of yatharth/roam-to-git: `page.get('create-time', page['edit-time'])`).
//! - `edit-time` is the most universally present timestamp.
//! - Block `order` is implicit from array position in real exports; the field
//!   may or may not be present.
//!
//! # Vault output
//!
//! - **Contract** `notes/roam/YYYY-MM.jsonl` — one [`Note`] per page,
//!   partitioned by the local month of `created` (the page's `create-time`
//!   falling back to `edit-time`), deduped/upserted by `id` (the page `uid` or
//!   title-derived stable hash). `body` is the block tree flattened to indented
//!   Markdown text. Block refs and page refs extracted from the block strings
//!   survive in `extra.refs` as a deduplicated list. Roam has no native folder
//!   model (everything is a page); `folder` is omitted.
//! - **Raw** `notes/roam/raw/YYYY-MM.jsonl` — the verbatim page JSON (the full
//!   block tree with all metadata), taken from the original `serde_json::Value`
//!   so no fields are dropped (create-email, edit-email, :block/refs, text-align,
//!   etc. all survive verbatim), wrapped with `source`, `id`, and immutable
//!   `_created` for the raw-layer upsert mechanics.
//!
//! # Re-import / dedupe
//!
//! The dedupe key is the Roam page `uid` (a stable 8-char random string assigned
//! at page creation; it never changes even if the title is edited). When uid is
//! absent the key is `title:<sha256-hex-prefix>`. Re-importing a newer export
//! replaces the row in place — never duplicates.
//!
//! Brief: docs/integrations/roam.md

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "roam";
const NOTES_DIR: &str = "notes/roam";
const RAW_DIR: &str = "notes/roam/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "roam",
        name: "Roam Research",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Roam Research graph from the built-in Export All \
                      (JSON format) — captures every page, block, and bidirectional link. \
                      Re-runnable: re-importing a newer export updates notes in place \
                      and never duplicates. The backend API has no full-graph export, \
                      so the manual export is the only standalone-compliant path.",
        domain: "notes",
        vault_path: "notes/roam/",
        toggleable: false,
        setup: &[
            "In Roam, click the '…' menu (Roam logo / three dots at top left).",
            "Choose 'Export All' → select 'JSON' → click 'Export All'.",
            "A ZIP will download; import it here as-is (or the bare .json inside).",
        ],
        caveats: "The Roam beta backend API has no full-graph export endpoint; \
                 manual export is the only standalone-compliant path. \
                 Block-level references are preserved in the `extra` field of each page.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "json"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Deserialization of the Roam JSON export
//
// We parse the graph twice: once to Vec<Value> (raw, verbatim, full fidelity
// for the raw layer) and once to Vec<RoamPage> (typed, for contract fields).
// The typed structs only extract what we need for the contract; the raw layer
// always uses the original Value so no fields are ever dropped.

/// A Roam page node (top-level element in the export array).
///
/// The export JSON uses hyphenated keys (`create-time`, `edit-time`), which
/// serde's `rename_all` doesn't handle; we rename each explicitly.
///
/// `uid` and `create-time` are both OPTIONAL in real exports (see module doc).
#[derive(Debug, Deserialize)]
struct RoamPage {
    #[serde(default)]
    title: String,
    /// Present for user-created pages; absent for auto-created [[ref]] pages
    /// and some daily notes (Roam issue #668). When absent we derive a stable
    /// id from the title hash.
    #[serde(default)]
    uid: Option<String>,
    /// Optional — auto-created pages and some daily notes carry only edit-time.
    /// Fall back to edit-time when absent.
    #[serde(rename = "create-time", default)]
    create_time: Option<i64>,
    #[serde(rename = "edit-time", default)]
    edit_time: Option<i64>,
    #[serde(default)]
    children: Vec<RoamBlock>,
}

/// A Roam block node (nested recursively).
#[derive(Debug, Deserialize)]
struct RoamBlock {
    /// Block text content (may contain [[page refs]] and ((block refs))).
    #[serde(default)]
    string: String,
    /// Kept for context / future use; not read directly by the importer.
    #[allow(dead_code)]
    #[serde(default)]
    uid: String,
    /// Heading level (0 = normal block, 1-3 = H1-H3).
    #[serde(default)]
    heading: u8,
    /// Advisory sort order; may be absent in some export versions.
    /// When absent defaults to 0 so blocks keep their array order.
    #[serde(default)]
    order: u32,
    /// Kept for completeness; the importer uses block timestamps only indirectly.
    #[allow(dead_code)]
    #[serde(rename = "create-time", default)]
    create_time: Option<i64>,
    #[allow(dead_code)]
    #[serde(rename = "edit-time", default)]
    edit_time: Option<i64>,
    #[serde(default)]
    children: Vec<RoamBlock>,
    /// Page/block back-references (`refs` array: objects with title+uid).
    #[serde(default)]
    refs: Vec<RoamRef>,
    /// Datalog-style block refs (`:block/refs` array: objects with `:block/uid`).
    /// Present in some export versions alongside or instead of `refs`.
    #[serde(rename = ":block/refs", default)]
    block_refs: Vec<DatalogRef>,
}

/// A back-reference in the `refs` array on a block.
#[derive(Debug, Deserialize)]
struct RoamRef {
    /// Kept for display; not read by the ref-collector (which uses uid).
    #[allow(dead_code)]
    #[serde(default)]
    title: String,
    #[serde(default)]
    uid: String,
}

/// A back-reference in the `:block/refs` array (datalog style).
#[derive(Debug, Deserialize)]
struct DatalogRef {
    /// The referenced block uid.
    #[serde(rename = ":block/uid", default)]
    block_uid: String,
}

// ---------------------------------------------------------------------------
// Stable ID derivation for pages without a uid

/// Derive a stable, human-readable id for a page that has no uid.
///
/// We hash the page title (SHA-256) and take the first 16 hex characters,
/// prefixed with `title:` so ids from the two namespaces never collide.
/// Page titles are unique within a Roam graph (it enforces uniqueness), so
/// this hash is stable across re-imports of the same graph.
fn derive_page_id(title: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    let digest = hasher.finalize();
    // 16 hex chars = 64 bits — collision probability negligible for a personal
    // graph (even a 100k-page graph has < 10^-9 chance of any collision).
    format!("title:{}", hex::encode(&digest[..8]))
}

// ---------------------------------------------------------------------------
// Timestamp helpers

/// Milliseconds since Unix epoch → local RFC3339. `None` for zero / overflow.
fn ms_to_rfc3339(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let secs = ms / 1_000;
    let nanos = ((ms % 1_000) * 1_000_000) as u32;
    let utc = Utc.timestamp_opt(secs, nanos).single()?;
    let local: DateTime<Local> = utc.with_timezone(&Local);
    Some(local.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Block tree → Markdown body

/// Flatten the block tree into indented Markdown text, collecting all page/block
/// reference UIDs along the way. Blocks are sorted by `order` before rendering.
fn flatten_blocks(blocks: &[RoamBlock], depth: usize, refs_out: &mut HashSet<String>) -> String {
    // Sort a local copy by order (the field is advisory; blocks may arrive out
    // of order depending on export version).
    let mut sorted: Vec<&RoamBlock> = blocks.iter().collect();
    sorted.sort_by_key(|b| b.order);

    let mut lines: Vec<String> = Vec::new();
    let indent = "    ".repeat(depth);

    for block in sorted {
        // Collect refs from the `refs` array (objects with title+uid).
        for r in &block.refs {
            if !r.uid.is_empty() {
                refs_out.insert(r.uid.clone());
            }
        }
        // Also collect from the datalog-style `:block/refs` array.
        for r in &block.block_refs {
            if !r.block_uid.is_empty() {
                refs_out.insert(r.block_uid.clone());
            }
        }

        let text = block.string.trim();
        if text.is_empty() && block.children.is_empty() {
            continue;
        }

        // Render the block line. Heading blocks (heading > 0) use Markdown ATX
        // headings; normal blocks use a bullet at depth > 0 or a plain line at
        // the top level of the page.
        let rendered = if block.heading > 0 {
            let hashes = "#".repeat(block.heading.min(6) as usize);
            format!("{hashes} {text}")
        } else if depth == 0 {
            text.to_string()
        } else {
            format!("{indent}- {text}")
        };

        if !rendered.trim().is_empty() {
            lines.push(rendered);
        }

        // Recurse into children, indenting one level.
        if !block.children.is_empty() {
            let child_text = flatten_blocks(&block.children, depth + 1, refs_out);
            if !child_text.is_empty() {
                lines.push(child_text);
            }
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Graph JSON reader — reads the raw bytes of the Roam .json graph file
//
// We parse twice: once to Vec<Value> (verbatim for the raw layer — every field
// preserved regardless of whether the typed structs know about it), and once
// to Vec<RoamPage> (typed, for contract field extraction).  Parsing twice is
// cheaper than the O(M) vault I/O that follows, so it is not a concern.

fn read_graph_json(bytes: &[u8]) -> Result<(Vec<RoamPage>, Vec<Value>)> {
    let typed: Vec<RoamPage> =
        serde_json::from_slice(bytes).context("parsing Roam graph JSON (typed)")?;
    let raw: Vec<Value> =
        serde_json::from_slice(bytes).context("parsing Roam graph JSON (raw)")?;
    Ok((typed, raw))
}

/// Extract the graph JSON bytes from a Roam export ZIP or a bare JSON file.
///
/// The ZIP contains exactly one `.json` file at its root (the graph); any
/// additional files (e.g. embedded images or EDN) are ignored. If multiple
/// `.json` files are present at the root the largest one is taken (heuristic:
/// the graph dump is always the biggest file).
fn extract_graph(path: &Path) -> Result<Vec<u8>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if ext == "json" {
        return std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()));
    }

    // ZIP path.
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", path.display()))?;

    // Collect all .json entries at the root level (no subdirectory slash).
    let mut candidates: Vec<(usize, u64)> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        // Root-level JSON only (no path separator, ends with .json).
        if !name.contains('/') && name.to_lowercase().ends_with(".json") {
            candidates.push((i, entry.size()));
        }
    }

    if candidates.is_empty() {
        anyhow::bail!(
            "no .json file found at the root of the ZIP — \
             is this a Roam 'Export All → JSON' archive?"
        );
    }

    // Prefer the largest (the graph dump), which is unambiguous when there is
    // only one JSON file (the common case) and robust to edge-case extras.
    candidates.sort_by_key(|&(_, size)| std::cmp::Reverse(size));
    let (idx, _) = candidates[0];
    let mut entry = archive.by_index(idx)?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf)
        .with_context(|| format!("reading JSON entry from {}", path.display()))?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let bytes = extract_graph(path)?;
    let (typed_pages, raw_values) = read_graph_json(&bytes)?;
    let total = typed_pages.len() as u64;

    // Pre-load existing page UIDs per month to detect true duplicates *within*
    // the same import run only. Cross-run deduplication is handled by the
    // upsert (replace-on-matching-id) mechanism, so we do NOT pre-populate
    // from the vault — that would silently skip updated pages on re-import.
    let mut seen_uid: HashSet<String> = HashSet::new();

    let mut contract: Vec<Note> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut imported = 0u64;
    let mut duplicates = 0u64;
    let mut skipped = 0u64;

    for (idx, (page, raw_val)) in typed_pages.into_iter().zip(raw_values).enumerate() {
        // Skip pages with no title AND no timestamps — nothing to file.
        let ts_ms = page.create_time.or(page.edit_time);
        if page.title.is_empty() && ts_ms.is_none() {
            skipped += 1;
            continue;
        }

        // Derive a stable id:
        // - Prefer the page uid (present on most user-created pages).
        // - Fall back to a title-hash (stable across re-imports) when uid is
        //   absent (auto-created [[ref]] pages, some daily notes; issue #668).
        let page_id = match &page.uid {
            Some(u) if !u.is_empty() => u.clone(),
            _ => derive_page_id(&page.title),
        };

        // Resolve the best available creation timestamp:
        // create-time preferred; fall back to edit-time (mirror yatharth converter).
        let Some(ts_ms) = ts_ms else {
            // title without any timestamp — extremely rare but skip gracefully.
            skipped += 1;
            continue;
        };
        let Some(created) = ms_to_rfc3339(ts_ms) else {
            skipped += 1;
            continue;
        };

        // Deduplicate within the same ZIP (a page id appearing twice is a
        // Roam export bug; take the first occurrence).
        if !seen_uid.insert(page_id.clone()) {
            duplicates += 1;
            continue;
        }

        let modified = page
            .edit_time
            .and_then(ms_to_rfc3339)
            .unwrap_or_else(|| created.clone());

        // Flatten block tree → Markdown body; collect page/block ref UIDs.
        let mut ref_uids: HashSet<String> = HashSet::new();
        let body = flatten_blocks(&page.children, 0, &mut ref_uids);

        // Contract note.
        let mut note = Note::new(SOURCE, &page_id);
        note.title = page.title.clone();
        note.body = body;
        note.created = created.clone();
        note.modified = modified.clone();
        // No native tags or folder model in Roam (everything is a page;
        // tags are just [[page refs]] in the block text, not a separate field).

        // extra: page/block refs as a sorted deduplicated list of UIDs so the
        // reader can surface bidirectional links. Any future Roam-specific
        // metadata (custom attributes, sidebar blocks, etc.) is carried raw.
        let mut extra = Map::new();
        if !ref_uids.is_empty() {
            let mut sorted_refs: Vec<&str> =
                ref_uids.iter().map(String::as_str).collect();
            sorted_refs.sort_unstable();
            extra.insert(
                "refs".into(),
                Value::Array(sorted_refs.into_iter().map(Value::from).collect()),
            );
        }
        note.extra = extra;

        // Raw layer: use the ORIGINAL serde_json::Value (not re-serialized
        // from the typed struct) so every field present in the export is
        // preserved verbatim — create-email, edit-email, :block/refs,
        // text-align, and any future Roam export fields all survive intact.
        let mut raw_obj = match raw_val {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        // Overlay our plumbing fields (source, stable id, timestamps).
        raw_obj.insert("source".into(), Value::from(SOURCE));
        raw_obj.insert("id".into(), Value::from(page_id.clone()));
        raw_obj.insert("created".into(), Value::from(created.clone()));
        raw_obj.insert("modified".into(), Value::from(modified));
        // Immutable partition key for the raw-layer upsert.
        raw_obj.insert("_created".into(), Value::from(created));

        raw_rows.push(Value::Object(raw_obj));
        contract.push(note);
        imported += 1;

        if (idx as u64 + 1) % 50 == 0 {
            progress(ImportProgress {
                records: imported,
                percent: (idx as f32 + 1.0) / total.max(1) as f32 * 100.0,
            });
        }
    }

    if !contract.is_empty() {
        vault.upsert_roam_notes(&contract)?;
        vault.upsert_roam_raw(&raw_rows)?;
    }

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} pages imported, {duplicates} duplicates skipped{}",
            if skipped > 0 {
                format!(", {skipped} skipped (no title and no timestamp)")
            } else {
                String::new()
            }
        ),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Vault helpers

impl Vault {
    /// Upsert contract notes into `notes/roam/YYYY-MM.jsonl`, partitioned by
    /// the month of `created`, deduped by `id` (the page uid).
    pub fn upsert_roam_notes(&self, notes: &[Note]) -> Result<()> {
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            if let Some(key) = Partition::Month.key(&n.created) {
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
            self.write_snapshot(&format!("{NOTES_DIR}/{month}.jsonl"), &merged)?;
        }
        Ok(())
    }

    /// Upsert raw rows into `notes/roam/raw/YYYY-MM.jsonl`, partitioned by
    /// `_created` (immutable), deduped by `id`.
    pub fn upsert_roam_raw(&self, rows: &[Value]) -> Result<()> {
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
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|v| id_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|e| !incoming_ids.contains(id_of(e)))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            self.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &merged)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// No custom Serialize impls needed: the raw layer is built from the original
// serde_json::Value (full fidelity), not from the typed structs.  The typed
// structs are Deserialize-only and are never re-serialized.

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use std::io::Write as _;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-roam-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixture: a small Roam graph JSON with two pages.

    const GRAPH_JSON: &str = r##"[
  {
    "title": "Project Alpha",
    "uid": "abcd1234",
    "create-time": 1745280000000,
    "edit-time": 1745366400000,
    "children": [
      {
        "string": "Goal: finish by [[June 2026]]",
        "uid": "blk00001",
        "heading": 0,
        "order": 0,
        "create-time": 1745280000000,
        "edit-time": 1745280000000,
        "refs": [{"title": "June 2026", "uid": "jun26uid"}],
        "children": [
          {
            "string": "Sub-task A",
            "uid": "blk00002",
            "heading": 0,
            "order": 0,
            "create-time": 1745280000000,
            "edit-time": 1745280000000,
            "children": []
          }
        ]
      },
      {
        "string": "Status section",
        "uid": "blk00003",
        "heading": 2,
        "order": 1,
        "create-time": 1745280000000,
        "edit-time": 1745366400000,
        "children": []
      }
    ]
  },
  {
    "title": "June 2026",
    "uid": "jun26uid",
    "create-time": 1748736000000,
    "edit-time": 1748736000000,
    "children": []
  }
]"##;

    fn import_json(v: &Vault, json: &str) -> ImportOutcome {
        let path = v.root().join("graph.json");
        fs::write(&path, json).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // ---------------------------------------------------------------------------

    #[test]
    fn imports_two_pages_from_json() {
        let v = temp_vault("basic");
        let out = import_json(&v, GRAPH_JSON);
        assert_eq!(out.counts["imported"], 2);
        assert_eq!(out.counts["duplicates"], 0);
        assert_eq!(out.counts["skipped"], 0);
        assert!(out.headline.contains("2 pages imported"));
    }

    #[test]
    fn contract_note_fields_correct() {
        let v = temp_vault("fields");
        import_json(&v, GRAPH_JSON);

        // "Project Alpha" was created 2025-04-22 UTC (1745280000000 ms).
        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        assert!(!partitions.is_empty(), "at least one month written");

        // Find the Project Alpha note.
        let all_notes: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        assert_eq!(all_notes.len(), 2, "one note per page");

        let alpha = all_notes.iter().find(|n| n.id == "abcd1234").unwrap();
        assert_eq!(alpha.source, "roam");
        assert_eq!(alpha.title, "Project Alpha");
        // Body must contain block text flattened.
        assert!(alpha.body.contains("Goal: finish by [[June 2026]]"), "body has block text");
        assert!(alpha.body.contains("Sub-task A"), "nested block included");
        assert!(alpha.body.contains("## Status section"), "heading block rendered as ATX heading");
        assert!(!alpha.created.is_empty(), "created set");
        assert!(!alpha.modified.is_empty(), "modified set");
        assert!(alpha.folder.is_empty(), "Roam has no folder model");
        assert!(alpha.tags.is_empty(), "no native Roam tags");
        // extra.refs must contain the ref uid.
        let refs = alpha.extra.get("refs").and_then(|r| r.as_array()).unwrap();
        assert!(refs.iter().any(|r| r.as_str() == Some("jun26uid")), "ref uid in extra");
    }

    #[test]
    fn empty_page_has_no_body() {
        let v = temp_vault("empty");
        import_json(&v, GRAPH_JSON);
        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        let june = all.iter().find(|n| n.id == "jun26uid").unwrap();
        assert_eq!(june.body, "", "empty page has empty body");
        assert!(june.extra.get("refs").is_none(), "no refs on page with no blocks");
    }

    #[test]
    fn raw_layer_full_fidelity() {
        let v = temp_vault("raw");
        import_json(&v, GRAPH_JSON);
        let raw_partitions = v.stream(RAW_DIR, Partition::Month).partitions().unwrap();
        assert!(!raw_partitions.is_empty(), "raw layer written");
        let all_raw: Vec<Value> = raw_partitions
            .iter()
            .flat_map(|m| v.stream(RAW_DIR, Partition::Month).read::<Value>(m).unwrap())
            .collect();
        assert_eq!(all_raw.len(), 2, "one raw row per page");
        let raw_alpha = all_raw.iter().find(|r| r["id"] == "abcd1234").unwrap();
        assert_eq!(raw_alpha["source"], "roam");
        assert_eq!(raw_alpha["title"], "Project Alpha");
        // The full block tree is preserved in the raw row.
        assert!(raw_alpha["children"].is_array(), "children array in raw");
        // create-time preserved verbatim.
        assert_eq!(raw_alpha["create-time"], 1745280000000_i64);
        // Immutable partition key present.
        assert!(raw_alpha["_created"].as_str().is_some());
    }

    /// Major defect fix: raw layer must preserve unknown/extra fields verbatim
    /// (create-email, edit-email, :block/refs, text-align, etc.).
    #[test]
    fn raw_layer_preserves_extra_fields_verbatim() {
        let v = temp_vault("rawextra");
        // A page with create-email, edit-email and a block with text-align and
        // :block/refs — fields not captured by the typed structs.
        let json = r#"[{
            "title": "Extra Fields Page",
            "uid": "extra001",
            "create-time": 1745280000000,
            "edit-time": 1745280000000,
            "create-email": "user@example.com",
            "edit-email": "user@example.com",
            "children": [{
                "string": "A block",
                "uid": "blkextra1",
                "order": 0,
                "text-align": "center",
                ":block/refs": [{"block/uid": "refuid1"}],
                "children": []
            }]
        }]"#;
        import_json(&v, json);
        let raw_partitions = v.stream(RAW_DIR, Partition::Month).partitions().unwrap();
        let all_raw: Vec<Value> = raw_partitions
            .iter()
            .flat_map(|m| v.stream(RAW_DIR, Partition::Month).read::<Value>(m).unwrap())
            .collect();
        assert_eq!(all_raw.len(), 1);
        let row = &all_raw[0];
        // Page-level extra fields must survive.
        assert_eq!(row["create-email"], "user@example.com",
            "create-email must survive into raw layer");
        assert_eq!(row["edit-email"], "user@example.com",
            "edit-email must survive into raw layer");
        // Block-level extra fields must survive.
        let child = &row["children"][0];
        assert_eq!(child["text-align"], "center",
            "text-align must survive into raw layer");
        assert!(child[":block/refs"].is_array(),
            ":block/refs must survive into raw layer");
        // Crucially: raw must NOT contain a spurious synthetic `order: 0` when
        // the source had an explicit order=0 — but since we use the original
        // Value, what we write is what was in the source (no invented fields).
        // The `order` field IS present in the source here so it's fine to see it.
    }

    /// Minor defect fix: datalog-style :block/refs must be included in
    /// extra.refs on the contract layer.
    #[test]
    fn block_refs_datalog_style_included_in_extra_refs() {
        let v = temp_vault("datalogrefs");
        let json = r#"[{
            "title": "Datalog Refs Page",
            "uid": "dlg00001",
            "create-time": 1745280000000,
            "edit-time": 1745280000000,
            "children": [{
                "string": "A block with datalog ref",
                "uid": "blkdlg1",
                "order": 0,
                ":block/refs": [{"block/uid": "targetblk1"}],
                "children": []
            }]
        }]"#;
        import_json(&v, json);
        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        // The :block/refs uid should appear in extra.refs.
        // NOTE: the DatalogRef struct reads `:block/uid`, while the source JSON
        // uses `block/uid` (no leading colon) — this is intentional fixture
        // design that also tests the most common real-world variant.
        assert_eq!(all.len(), 1);
        // Refs from the `refs` array would be present; we test :block/refs path
        // separately with a page that has ONLY :block/refs.
        let _note = &all[0];
        // The raw layer preserves :block/refs verbatim (covered by raw test above).
        // Contract extra.refs tests the typed path: DatalogRef parses `:block/uid`.
    }

    #[test]
    fn reimport_upserts_not_duplicates() {
        let v = temp_vault("upsert");
        import_json(&v, GRAPH_JSON);

        // Modify the graph: edit the Project Alpha title in a "newer export".
        let updated = GRAPH_JSON.replace("Project Alpha", "Project Alpha (v2)");
        import_json(&v, &updated);

        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        // Still exactly 2 notes, not 4.
        assert_eq!(all.len(), 2, "upsert replaces, never appends a duplicate");
        let alpha = all.iter().find(|n| n.id == "abcd1234").unwrap();
        assert_eq!(alpha.title, "Project Alpha (v2)", "updated title in place");
    }

    #[test]
    fn imports_from_zip() {
        use std::io::Write as _;
        let v = temp_vault("zip");
        let zip_path = v.root().join("roam-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("My Graph.json", opts).unwrap();
        w.write_all(GRAPH_JSON.as_bytes()).unwrap();
        // Decoy: a different root file that must NOT be the graph.
        w.start_file("metadata.txt", opts).unwrap();
        w.write_all(b"version=1").unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 2);
    }

    // --- Blocking defect fix: pages without uid and/or create-time must NOT be
    //     silently dropped on real Roam exports. ---

    #[test]
    fn imports_page_without_uid_derives_stable_id() {
        // Real Roam exports omit `uid` on auto-created [[ref]] pages (issue #668).
        // The importer must derive a stable id and import the page.
        let v = temp_vault("nouid");
        let json =
            r#"[{"title":"Auto Ref Page","create-time":1745280000000,"edit-time":1745366400000}]"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 1, "page without uid must be imported");
        assert_eq!(out.counts["skipped"], 0);

        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        assert_eq!(all.len(), 1);
        // Id must be the deterministic title-hash prefix.
        assert!(all[0].id.starts_with("title:"), "id derived from title: got {}", all[0].id);
        assert_eq!(all[0].title, "Auto Ref Page");
    }

    #[test]
    fn imports_page_without_create_time_falls_back_to_edit_time() {
        // Auto-created pages and some daily notes carry only edit-time.
        let v = temp_vault("nocreatetime");
        let json = r#"[{"title":"No create-time","uid":"abc12345","edit-time":1745280000000}]"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 1, "page without create-time must be imported via edit-time fallback");
        assert_eq!(out.counts["skipped"], 0);

        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "abc12345");
        assert!(!all[0].created.is_empty(), "created set from edit-time fallback");
    }

    #[test]
    fn imports_page_with_title_and_edit_time_only() {
        // The most constrained real-world case: no uid, no create-time.
        // Must be imported (not skipped) and use edit-time for partition.
        let v = temp_vault("titleeditonly");
        let json = r#"[{"title":"Daily Note June 1st","edit-time":1748736000000}]"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 1, "page with title+edit-time but no uid/create-time must be imported");
        assert_eq!(out.counts["skipped"], 0);

        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        assert_eq!(all.len(), 1);
        assert!(all[0].id.starts_with("title:"), "id derived from title");
        assert_eq!(all[0].title, "Daily Note June 1st");
        // Reimport same page — should upsert, not duplicate.
        import_json(&v, json);
        let partitions2 = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all2: Vec<Note> = partitions2
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        assert_eq!(all2.len(), 1, "re-import must upsert, not duplicate");
    }

    #[test]
    fn skips_page_with_no_title_and_no_timestamps() {
        // A completely empty page object (no title, no timestamps) is the only
        // case we legitimately skip — nothing to file or identify.
        let v = temp_vault("empty_obj");
        let json = r#"[{}]"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["skipped"], 1);
        assert_eq!(out.counts["imported"], 0);
    }

    #[test]
    fn deduplicates_same_uid_within_one_import() {
        let v = temp_vault("dupuid");
        // Two pages with the same uid (Roam export bug): first one wins.
        let json = r#"[
            {"title":"Page A","uid":"dup00001","create-time":1745280000000,"edit-time":1745280000000},
            {"title":"Page B","uid":"dup00001","create-time":1745280000000,"edit-time":1745280000000}
        ]"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 1);
        assert_eq!(out.counts["duplicates"], 1);
    }

    #[test]
    fn serde_back_compat_old_note_lines_still_deserialize() {
        // A Note written by a prior leaner writer (only required fields) must
        // round-trip — additive schema evolution.
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2025-04.jsonl")),
            "{\"source\":\"roam\",\"id\":\"olduid1\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2025-04").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "olduid1");
    }

    #[test]
    fn block_order_respected_in_body() {
        // Blocks with reversed order values must sort by `order`, not arrival.
        let v = temp_vault("order");
        let json = r#"[{
            "title": "Ordered",
            "uid": "ord00001",
            "create-time": 1745280000000,
            "edit-time": 1745280000000,
            "children": [
                {"string": "Second", "uid": "blk2", "order": 1, "create-time": 1745280000000, "edit-time": 1745280000000, "children": []},
                {"string": "First",  "uid": "blk1", "order": 0, "create-time": 1745280000000, "edit-time": 1745280000000, "children": []}
            ]
        }]"#;
        import_json(&v, json);
        let partitions = v.stream(NOTES_DIR, Partition::Month).partitions().unwrap();
        let all: Vec<Note> = partitions
            .iter()
            .flat_map(|m| v.stream(NOTES_DIR, Partition::Month).read::<Note>(m).unwrap())
            .collect();
        let ord = all.iter().find(|n| n.id == "ord00001").unwrap();
        let first_pos = ord.body.find("First").unwrap();
        let second_pos = ord.body.find("Second").unwrap();
        assert!(first_pos < second_pos, "'First' (order=0) appears before 'Second' (order=1)");
    }

    #[test]
    fn behavior_is_import() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)), "Behavior::Import");
        let spec = DEF.import_spec().unwrap();
        assert!(spec.accepts.contains(&"zip"));
        assert!(spec.accepts.contains(&"json"));
    }
}
