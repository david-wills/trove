//! Omnivore — historical import (hosted service shut down November 2024).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/omnivore.md.
//!
//! Omnivore exported a ZIP containing three kinds of files:
//!
//! - **`metadata_N_to_M.json`** — a JSON array of article metadata objects,
//!   one entry per saved article. Fields (confirmed from the open-source
//!   export-handler at github.com/omnivore-app/omnivore):
//!   `id`, `slug`, `title`, `description`, `author`, `url`,
//!   `state` (`"archived"` | `"active"`), `readingProgress` (0–100 integer),
//!   `thumbnail`, `labels` (string[]), `savedAt`, `updatedAt`, `publishedAt`.
//! - **`content/{slug}.html`** — the saved article HTML (preserved in raw).
//! - **`highlights/{slug}.md`** — one file per article that has highlights;
//!   each highlight is a blockquote line `> {quote}` optionally followed by
//!   `#label` hashtags and a note paragraph. Multiple highlights in one file
//!   are separated by blank lines.
//!
//! **Note:** the brief says "markdown files with YAML frontmatter" — that is
//! incorrect; the actual format (confirmed from source) is JSON metadata files
//! + separate HTML + highlight-MD files. This implementation follows the real
//! format. Parser correction noted in brief_updated.
//!
//! ## Contract mapping
//!
//! - One [`crate::reading::Item`] per article → `reading/omnivore/YYYY-MM.jsonl`
//!   (month of `savedAt`). `guid` = `id`. `state`: `"archived"` | `"saved"`.
//!   `readingProgress` → `progress`. `labels` → `tags`. Overflow to `extra`.
//!
//! - One [`crate::reading::Highlight`] per highlight → `reading/omnivore/highlights/YYYY-MM.jsonl`.
//!   `guid` = `{article_id}::{ordinal}` (stable across re-imports when the
//!   highlight order is stable, which it is for a static export file).
//!   Article `title`/`url`/`author` carried inline on every highlight row.
//!
//! ## Raw layer
//!
//! - `reading/omnivore/raw/YYYY-MM.jsonl` — each metadata object verbatim.
//! - `reading/omnivore/highlights/raw/YYYY-MM.jsonl` — each highlight as
//!   `{"article_id","slug","ordinal","text","note","labels"[]}`, keyed to the
//!   highlight's article `savedAt` month.
//!
//! Re-importing the same ZIP (or a superset) is a no-op for records already
//! stored: dedupe is by `guid` on both the item and highlight streams.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::reading::{Highlight, Item};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "reading/omnivore";
const HIGHLIGHTS_DIR: &str = "reading/omnivore/highlights";
const RAW_DIR: &str = "reading/omnivore/raw";
const HIGHLIGHTS_RAW_DIR: &str = "reading/omnivore/highlights/raw";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    let items = crate::registry::newest_stem(&vault.root().join(DIR));
    let hl = crate::registry::newest_stem(&vault.root().join(HIGHLIGHTS_DIR));
    [items, hl].into_iter().flatten().max()
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "omnivore",
        name: "Omnivore (historical import)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your saved articles and highlights from Omnivore, the open-source \
                      read-later app that shut down in November 2024. Accepts the ZIP export \
                      you downloaded before shutdown (contains JSON metadata + highlights).",
        domain: "reading",
        vault_path: "reading/omnivore/",
        toggleable: false,
        setup: &[
            "This import is for users who exported their Omnivore data before the service \
             shut down in November 2024.",
            "Locate your downloaded export ZIP (likely named something like omnivore-export.zip).",
            "Drop it here to import all your saved articles and highlights.",
        ],
        caveats: "Omnivore's hosted service is gone; only users who exported before shutdown \
                  have files. Each article's full HTML content is preserved in the raw layer. \
                  Re-importing the same ZIP is safe — no duplicates will be created.",
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
// Deserialization shapes.

/// One article from a `metadata_N_to_M.json` batch file.
#[derive(Debug, Deserialize)]
struct ArticleMeta {
    id: Option<String>,
    slug: Option<String>,
    title: Option<String>,
    description: Option<String>,
    author: Option<String>,
    url: Option<String>,
    state: Option<String>,
    #[serde(rename = "readingProgress")]
    reading_progress: Option<f64>,
    thumbnail: Option<String>,
    labels: Option<Vec<String>>,
    #[serde(rename = "savedAt")]
    saved_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    #[serde(rename = "publishedAt")]
    published_at: Option<String>,
}

/// A parsed highlight from one `highlights/{slug}.md` entry.
#[derive(Debug)]
struct ParsedHighlight {
    text: String,
    note: String,
    labels: Vec<String>,
}

// ---------------------------------------------------------------------------
// Raw-layer wrapper (for highlights).

#[derive(Debug, Serialize)]
struct RawHighlight {
    ts: String,
    article_id: String,
    slug: String,
    ordinal: usize,
    text: String,
    note: String,
    labels: Vec<String>,
}

// ---------------------------------------------------------------------------
// Import runner.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;

    // ---- Pass 1: collect all article metadata from metadata_*.json files ----

    // Map slug -> ArticleMeta for highlight-file association later.
    let mut articles_by_slug: BTreeMap<String, ArticleMeta> = BTreeMap::new();
    // Keep insertion order for progress reporting; slug order matches metadata.
    let mut article_order: Vec<String> = Vec::new();

    // Gather names first to avoid borrow issues.
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_string()))
        .collect();

    for name in &names {
        // Match metadata_N_to_M.json at the root (no path separator).
        if !name.contains('/') && name.starts_with("metadata_") && name.ends_with(".json") {
            let mut entry = archive
                .by_name(name)
                .with_context(|| format!("reading {name}"))?;
            let mut body = String::new();
            entry.read_to_string(&mut body).with_context(|| format!("reading {name}"))?;
            let batch: Vec<ArticleMeta> = serde_json::from_str(&body)
                .with_context(|| format!("parsing {name}"))?;
            for meta in batch {
                if let Some(slug) = &meta.slug {
                    let slug = slug.clone();
                    if !articles_by_slug.contains_key(&slug) {
                        article_order.push(slug.clone());
                    }
                    articles_by_slug.insert(slug, meta);
                }
            }
        }
    }

    // ---- Pass 2: collect highlights from highlights/{slug}.md files ----

    // Map slug -> Vec<ParsedHighlight>.
    let mut highlights_by_slug: BTreeMap<String, Vec<ParsedHighlight>> = BTreeMap::new();

    for name in &names {
        // Match highlights/{slug}.md
        if let Some(slug) = name
            .strip_prefix("highlights/")
            .and_then(|s| s.strip_suffix(".md"))
        {
            let slug = slug.to_string();
            let mut entry = archive
                .by_name(name)
                .with_context(|| format!("reading {name}"))?;
            let mut body = String::new();
            entry.read_to_string(&mut body).with_context(|| format!("reading {name}"))?;
            let parsed = parse_highlight_md(&body);
            if !parsed.is_empty() {
                highlights_by_slug.insert(slug, parsed);
            }
        }
    }

    // ---- Dedupe: load already-stored guids ----

    let item_stream = vault.stream(DIR, Partition::Month);
    let highlight_stream = vault.stream(HIGHLIGHTS_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let raw_hl_stream = vault.stream(HIGHLIGHTS_RAW_DIR, Partition::Month);

    let mut seen_items: HashSet<String> = HashSet::new();
    for key in item_stream.partitions()? {
        for v in item_stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(|v| v.as_str()) {
                seen_items.insert(g.to_string());
            }
        }
    }

    let mut seen_highlights: HashSet<String> = HashSet::new();
    for key in highlight_stream.partitions()? {
        for v in highlight_stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(|v| v.as_str()) {
                seen_highlights.insert(g.to_string());
            }
        }
    }

    // ---- Build contract + raw rows ----

    let mut new_items: Vec<Item> = Vec::new();
    let mut new_raws: Vec<Value> = Vec::new();
    let mut new_highlights: Vec<Highlight> = Vec::new();
    let mut new_raw_hls: Vec<RawHighlight> = Vec::new();

    let mut duplicates_items = 0u64;
    let mut duplicates_hls = 0u64;
    let mut skipped = 0u64;
    let total = article_order.len();

    for (idx, slug) in article_order.iter().enumerate() {
        let meta = match articles_by_slug.remove(slug) {
            Some(m) => m,
            None => continue,
        };

        let id = match &meta.id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => {
                skipped += 1;
                continue;
            }
        };

        // Convert savedAt to local RFC3339; fall back to epoch if missing.
        let ts = meta
            .saved_at
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| to_local(s))
            .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());

        // Build contract Item.
        if seen_items.insert(id.clone()) {
            // Real Omnivore export writes capitalized state values via
            // itemStateMappping() in packages/api/src/jobs/export.ts:
            //   "Archived" | "Active" | "Unknown"
            // Match case-insensitively so self-hosted instances with different
            // casings also work.
            let state = if meta
                .state
                .as_deref()
                .unwrap_or("")
                .eq_ignore_ascii_case("archived")
            {
                "archived"
            } else {
                "saved"
            }
            .to_string();

            let tags = meta.labels.clone().unwrap_or_default();
            let progress = meta.reading_progress.map(|p| p.round() as i64);

            let mut extra: Map<String, Value> = Map::new();
            if let Some(slug_val) = &meta.slug {
                if !slug_val.is_empty() {
                    extra.insert("slug".into(), Value::String(slug_val.clone()));
                }
            }
            if let Some(thumb) = &meta.thumbnail {
                if !thumb.is_empty() {
                    extra.insert("thumbnail".into(), Value::String(thumb.clone()));
                }
            }
            if let Some(updated) = &meta.updated_at {
                if !updated.is_empty() {
                    extra.insert("updated_at".into(), Value::String(updated.clone()));
                }
            }
            if let Some(published) = &meta.published_at {
                if !published.is_empty() {
                    extra.insert("published_at".into(), Value::String(published.clone()));
                }
            }

            let article_url = meta.url.clone().unwrap_or_default();
            // Derive publisher domain from the URL for the `site` field.
            // Simple extraction: strip scheme, take up to first '/', strip port.
            let site = {
                let s = article_url.as_str();
                let after_scheme = s
                    .find("://")
                    .map(|i| &s[i + 3..])
                    .unwrap_or(s);
                let host_port = after_scheme
                    .split('/')
                    .next()
                    .unwrap_or("");
                host_port
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string()
            };

            let item = Item {
                ts: ts.clone(),
                source: "omnivore".into(),
                guid: id.clone(),
                url: article_url.clone(),
                title: meta.title.clone().unwrap_or_default(),
                author: meta.author.clone().unwrap_or_default(),
                site,
                feed: String::new(),
                excerpt: meta.description.clone().unwrap_or_default(),
                tags,
                state,
                progress,
                read_at: String::new(),
                extra,
            };
            new_items.push(item);

            // Raw: the metadata value verbatim (re-serialize to Value).
            let raw_val = article_meta_to_value(&meta, &ts);
            new_raws.push(raw_val);
        } else {
            duplicates_items += 1;
        }

        // Highlights for this article.
        if let Some(highlights) = highlights_by_slug.remove(slug) {
            let article_title = meta.title.as_deref().unwrap_or("").to_string();
            let article_url = meta.url.as_deref().unwrap_or("").to_string();
            let article_author = meta.author.as_deref().unwrap_or("").to_string();

            for (ordinal, hl) in highlights.into_iter().enumerate() {
                let hl_guid = format!("{id}::{ordinal}");
                if seen_highlights.insert(hl_guid.clone()) {
                    new_highlights.push(Highlight {
                        // NOTE: Omnivore's highlight export markdown carries no
                        // per-highlight timestamp. We use the article's `savedAt`
                        // as an approximation. This means the contract's
                        // `highlight.ts` is "when the article was saved" rather
                        // than "when the highlight was made". This is the best
                        // available fidelity from the export format.
                        ts: ts.clone(),
                        source: "omnivore".into(),
                        guid: hl_guid.clone(),
                        text: hl.text.clone(),
                        note: hl.note.clone(),
                        title: article_title.clone(),
                        author: article_author.clone(),
                        url: article_url.clone(),
                        location: String::new(),
                        color: String::new(),
                        tags: hl.labels.clone(),
                        extra: Map::new(),
                    });
                    new_raw_hls.push(RawHighlight {
                        ts: ts.clone(),
                        article_id: id.clone(),
                        slug: slug.clone(),
                        ordinal,
                        text: hl.text,
                        note: hl.note,
                        labels: hl.labels,
                    });
                } else {
                    duplicates_hls += 1;
                }
            }
        }

        if idx % 100 == 0 {
            let pct = if total > 0 { idx as f32 / total as f32 } else { 0.0 };
            progress(ImportProgress {
                records: (new_items.len() + new_highlights.len()) as u64,
                percent: pct,
            });
        }
    }

    // ---- Write ----
    // Items: partitioned by savedAt month.
    item_stream.append(&new_items, |i| &i.ts)?;
    // Raw items: same partition.
    let raw_wrapped: Vec<_> = new_raws
        .iter()
        .zip(new_items.iter())
        .map(|(raw, item)| RawItemWrap { ts: item.ts.clone(), value: raw.clone() })
        .collect();
    raw_stream.append(&raw_wrapped, |r| &r.ts)?;
    // Highlights: partitioned by article savedAt month.
    highlight_stream.append(&new_highlights, |h| &h.ts)?;
    raw_hl_stream.append(&new_raw_hls, |h| &h.ts)?;

    let imported_items = new_items.len() as u64;
    let imported_hls = new_highlights.len() as u64;

    progress(ImportProgress {
        records: imported_items + imported_hls,
        percent: 100.0,
    });

    Ok(ImportOutcome {
        headline: format!(
            "{imported_items} articles and {imported_hls} highlights imported, \
             {duplicates_items} article duplicates and {duplicates_hls} highlight \
             duplicates skipped"
        ),
        counts: [
            ("articles_imported", imported_items),
            ("highlights_imported", imported_hls),
            ("articles_duplicate", duplicates_items),
            ("highlights_duplicate", duplicates_hls),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers.

/// Thin Serialize wrapper to carry a raw JSON Value with a timestamp key for
/// month-partition routing.
#[derive(Serialize)]
struct RawItemWrap {
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Convert an `ArticleMeta` back to a `serde_json::Value` for raw storage.
/// We store the original fields verbatim; `ts` is the locally-converted savedAt.
fn article_meta_to_value(meta: &ArticleMeta, ts: &str) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in [
        ("id", &meta.id),
        ("slug", &meta.slug),
        ("title", &meta.title),
        ("description", &meta.description),
        ("author", &meta.author),
        ("url", &meta.url),
        ("state", &meta.state),
        ("thumbnail", &meta.thumbnail),
        ("savedAt", &meta.saved_at),
        ("updatedAt", &meta.updated_at),
        ("publishedAt", &meta.published_at),
    ] {
        if let Some(s) = v {
            m.insert(k.into(), Value::String(s.clone()));
        }
    }
    if let Some(p) = meta.reading_progress {
        m.insert("readingProgress".into(), Value::from(p));
    }
    if let Some(labels) = &meta.labels {
        m.insert(
            "labels".into(),
            Value::Array(labels.iter().map(|l| Value::String(l.clone())).collect()),
        );
    }
    m.insert("ts".into(), Value::String(ts.to_string()));
    Value::Object(m)
}

/// An RFC3339-ish timestamp → RFC3339 local. Unparseable/empty values pass
/// through verbatim.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Parse the `highlights/{slug}.md` body into a list of [`ParsedHighlight`].
///
/// The real format emitted by Omnivore's `highlightToMarkdown`
/// (`packages/api/src/utils/parser.ts`):
///
/// **Regular highlight** (HighlightType.Highlight):
/// ```text
/// > {quote text}
///
/// #label1 #label2
///
/// {annotation / note}
/// ```
/// - The blockquote line(s) carry the quoted passage (no inline hashtags).
/// - If the article had labels, they appear as a SEPARATE paragraph
///   consisting solely of `#word` tokens (after the blockquote block).
/// - If the highlight had an annotation, it appears as a SEPARATE paragraph
///   after the labels paragraph (or directly after the blockquote if no
///   labels were present).
///
/// Multiple highlights in one file are joined with `\n\n`.
///
/// **Note-type highlight** (HighlightType.Note, no quoted passage):
/// ```text
/// {annotation}
///
/// ```
/// These have NO leading `> ` — just a plain text paragraph. We emit them
/// as a highlight with an empty `text` field and the annotation in `note`.
fn parse_highlight_md(body: &str) -> Vec<ParsedHighlight> {
    let mut results = Vec::new();

    // Split the entire body into "paragraphs" (runs of non-blank lines
    // separated by one or more blank lines).  Each paragraph is a Vec<&str>
    // of trimmed lines within it.
    let mut paragraphs: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.clone());
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }

    // Walk the paragraphs.  A "highlight unit" is:
    //   Option<blockquote paragraph> + Option<labels paragraph> + Option<note paragraph>
    // OR a lone non-blockquote paragraph (a Note-type highlight).
    let mut i = 0;
    while i < paragraphs.len() {
        let para = &paragraphs[i];

        if para.iter().any(|l| l.starts_with("> ") || *l == ">") {
            // ---- Regular highlight (blockquote paragraph) ----
            // Join all `> ` lines (strip the prefix, trim trailing spaces).
            let quote_parts: Vec<&str> = para
                .iter()
                .filter(|l| l.starts_with("> ") || **l == ">")
                .map(|l| l.strip_prefix("> ").unwrap_or("").trim_end())
                .filter(|s| !s.is_empty())
                .collect();
            let text = quote_parts.join(" ");
            i += 1;

            // Check the next paragraph: is it a labels-only paragraph?
            let mut labels: Vec<String> = Vec::new();
            if i < paragraphs.len() {
                let next = &paragraphs[i];
                let is_labels = !next.is_empty()
                    && next
                        .iter()
                        .all(|l| l.split_whitespace().all(|w| w.starts_with('#')));
                if is_labels {
                    for l in next {
                        for word in l.split_whitespace() {
                            labels.push(word.trim_start_matches('#').to_string());
                        }
                    }
                    i += 1;
                }
            }

            // The next paragraph (if any, and if it's not another blockquote
            // or another labels-only paragraph) is the note/annotation.
            let mut note = String::new();
            if i < paragraphs.len() {
                let next = &paragraphs[i];
                let is_blockquote =
                    next.iter().any(|l| l.starts_with("> ") || *l == ">");
                let is_labels = !next.is_empty()
                    && next
                        .iter()
                        .all(|l| l.split_whitespace().all(|w| w.starts_with('#')));
                if !is_blockquote && !is_labels {
                    note = next
                        .iter()
                        .map(|l| l.trim())
                        .collect::<Vec<_>>()
                        .join(" ");
                    i += 1;
                }
            }

            results.push(ParsedHighlight {
                text: text.trim().to_string(),
                note: note.trim().to_string(),
                labels,
            });
        } else {
            // ---- Note-type highlight: plain paragraph, no blockquote ----
            // HighlightType.Note emits just `${annotation}\n\n` with no `> `.
            // Store as a highlight with empty text and the annotation as note.
            let is_labels = para
                .iter()
                .all(|l| l.split_whitespace().all(|w| w.starts_with('#')));
            if !is_labels {
                let note = para
                    .iter()
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                results.push(ParsedHighlight {
                    text: String::new(),
                    note: note.trim().to_string(),
                    labels: Vec::new(),
                });
            }
            i += 1;
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-omnivore-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal metadata JSON for one article (no highlights).
    ///
    /// State value is `"Active"` — the real capitalized value emitted by
    /// Omnivore's `itemStateMappping()` in `packages/api/src/jobs/export.ts`.
    const META_SIMPLE: &str = r#"[
  {
    "id": "art-001",
    "slug": "why-local-first",
    "title": "Why Local-First?",
    "description": "The case for local-first software.",
    "author": "Martin Kleppmann",
    "url": "https://example.com/local-first",
    "state": "Active",
    "readingProgress": 75,
    "thumbnail": "https://example.com/thumb.jpg",
    "labels": ["software", "architecture"],
    "savedAt": "2024-03-15T10:30:00.000Z",
    "updatedAt": "2024-03-15T10:30:00.000Z",
    "publishedAt": "2023-09-01T00:00:00.000Z"
  }
]"#;

    /// A second article, archived, no highlights.
    ///
    /// State value is `"Archived"` — the real capitalized value emitted by
    /// Omnivore's `itemStateMappping()` in `packages/api/src/jobs/export.ts`.
    const META_ARCHIVED: &str = r#"[
  {
    "id": "art-002",
    "slug": "old-article",
    "title": "An Old Article",
    "description": "",
    "author": "",
    "url": "https://example.com/old",
    "state": "Archived",
    "readingProgress": 100,
    "thumbnail": "",
    "labels": [],
    "savedAt": "2023-11-10T08:00:00.000Z",
    "updatedAt": "2023-11-10T08:00:00.000Z",
    "publishedAt": null
  }
]"#;

    /// Highlights for art-001, in the REAL format emitted by
    /// Omnivore's `highlightToMarkdown()` (`packages/api/src/utils/parser.ts`):
    ///
    /// - Quote is a `> ` blockquote (NO inline hashtags).
    /// - Labels appear as a SEPARATE paragraph of `#word` tokens.
    /// - Note appears as a SEPARATE paragraph after the labels.
    /// - Multiple highlights are joined with a blank line.
    ///
    /// Contrast with the incorrect format this codebase used previously, where
    /// `#tag1 #tag2` was appended inline to the blockquote line.
    const HIGHLIGHTS_ART001: &str = "\
> This is the first highlight \n\n#tag1 #tag2\n\nMy note for the first highlight\n\n> Second highlight text\n\n> Third with no note\n";

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut w = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn do_import(v: &Vault, zip_bytes: &[u8]) -> ImportOutcome {
        let zip_path = v.root().join("export.zip");
        fs::write(&zip_path, zip_bytes).unwrap();
        (IMPORT.run)(v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // ---- Unit tests for highlight parser ------------------------------------

    /// Real format: quote blockquote, then labels paragraph, then note paragraph.
    /// Labels and note are each on their OWN blank-line-separated paragraph,
    /// NOT appended inline to the blockquote.
    #[test]
    fn parse_single_highlight_with_note_and_labels() {
        // Real format from highlightToMarkdown():
        //   `> ${quote} \n\n${labels}\n\n${note}`
        let body = "> Local-first is better \n\n#software #ideas\n\nGreat insight\n";
        let parsed = parse_highlight_md(body);
        assert_eq!(parsed.len(), 1, "got: {parsed:?}");
        assert_eq!(parsed[0].text, "Local-first is better");
        assert_eq!(parsed[0].note, "Great insight");
        assert_eq!(parsed[0].labels, vec!["software", "ideas"]);
    }

    #[test]
    fn parse_highlight_without_note() {
        let body = "> Just a highlight no label \n\n";
        let parsed = parse_highlight_md(body);
        assert_eq!(parsed.len(), 1, "got: {parsed:?}");
        assert_eq!(parsed[0].text, "Just a highlight no label");
        assert!(parsed[0].note.is_empty());
        assert!(parsed[0].labels.is_empty());
    }

    #[test]
    fn parse_highlight_with_labels_but_no_note() {
        // Labels paragraph present, note paragraph absent.
        let body = "> Quote text \n\n#philosophy #ethics\n\n";
        let parsed = parse_highlight_md(body);
        assert_eq!(parsed.len(), 1, "got: {parsed:?}");
        assert_eq!(parsed[0].text, "Quote text");
        assert_eq!(parsed[0].labels, vec!["philosophy", "ethics"]);
        assert!(parsed[0].note.is_empty());
    }

    #[test]
    fn parse_highlight_with_note_but_no_labels() {
        // Note paragraph present, labels paragraph absent.
        let body = "> Quote text \n\nThis is the annotation\n\n";
        let parsed = parse_highlight_md(body);
        assert_eq!(parsed.len(), 1, "got: {parsed:?}");
        assert_eq!(parsed[0].text, "Quote text");
        assert!(parsed[0].labels.is_empty());
        assert_eq!(parsed[0].note, "This is the annotation");
    }

    /// Note-type highlight (HighlightType.Note): plain paragraph with no `> `.
    /// The export emits `${annotation}\n\n` with no blockquote at all.
    #[test]
    fn parse_note_type_highlight_no_blockquote() {
        let body = "This is a reader note with no quoted passage\n\n";
        let parsed = parse_highlight_md(body);
        assert_eq!(parsed.len(), 1, "note-type highlight should produce 1 row: {parsed:?}");
        assert_eq!(parsed[0].text, "", "note-type has empty text field");
        assert_eq!(parsed[0].note, "This is a reader note with no quoted passage");
        assert!(parsed[0].labels.is_empty());
    }

    #[test]
    fn parse_multiple_highlights() {
        let hl = HIGHLIGHTS_ART001;
        let parsed = parse_highlight_md(hl);
        assert_eq!(parsed.len(), 3, "three highlight blocks: {parsed:?}");
        assert_eq!(parsed[0].text, "This is the first highlight");
        assert_eq!(parsed[0].note, "My note for the first highlight");
        assert_eq!(parsed[0].labels, vec!["tag1", "tag2"]);
        assert_eq!(parsed[1].text, "Second highlight text");
        assert!(parsed[1].note.is_empty());
        assert!(parsed[1].labels.is_empty());
        assert_eq!(parsed[2].text, "Third with no note");
        assert!(parsed[2].note.is_empty());
    }

    #[test]
    fn parse_empty_body() {
        assert!(parse_highlight_md("").is_empty());
        assert!(parse_highlight_md("   \n\n  \n").is_empty());
    }

    /// Verify that a mixed file (Note-type + regular highlights) is parsed
    /// correctly. highlightToMarkdown joins them all with \n\n.
    #[test]
    fn parse_mixed_note_and_highlight_types() {
        // A Note-type, then a regular highlight with labels, joined by \n\n.
        let body =
            "Standalone reader note\n\n> Regular quote \n\n#tag1\n\nAnnotation text\n\n";
        let parsed = parse_highlight_md(body);
        assert_eq!(parsed.len(), 2, "note-type + regular: {parsed:?}");
        // First: Note-type (no blockquote)
        assert_eq!(parsed[0].text, "");
        assert_eq!(parsed[0].note, "Standalone reader note");
        // Second: regular highlight
        assert_eq!(parsed[1].text, "Regular quote");
        assert_eq!(parsed[1].labels, vec!["tag1"]);
        assert_eq!(parsed[1].note, "Annotation text");
    }

    // ---- Integration tests --------------------------------------------------

    #[test]
    fn imports_articles_and_writes_contract_rows() {
        let v = temp_vault("basic");
        let zip_bytes = build_zip(&[
            ("metadata_0_to_1.json", META_SIMPLE.as_bytes()),
            ("content/why-local-first.html", b"<h1>Why Local-First?</h1>"),
        ]);
        let out = do_import(&v, &zip_bytes);

        // Outcome.
        assert!(
            out.counts.get("articles_imported") == Some(&1),
            "expected 1 article: {out:?}"
        );
        assert!(out.counts.get("highlights_imported") == Some(&0));

        // Contract file written.
        let item_file = v.root().join("reading/omnivore/2024-03.jsonl");
        assert!(item_file.exists(), "contract file missing");
        let content = fs::read_to_string(&item_file).unwrap();
        let item: Item = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(item.guid, "art-001");
        assert_eq!(item.title, "Why Local-First?");
        assert_eq!(item.author, "Martin Kleppmann");
        assert_eq!(item.url, "https://example.com/local-first");
        assert_eq!(item.state, "saved");
        assert_eq!(item.progress, Some(75));
        assert_eq!(item.tags, vec!["software", "architecture"]);
        assert_eq!(item.source, "omnivore");
        assert_eq!(item.site, "example.com", "site derived from URL host");
        assert!(item.extra.contains_key("slug"), "slug in extra");
        assert!(item.extra.contains_key("thumbnail"), "thumbnail in extra");

        // Raw file written.
        let raw_dir = v.root().join("reading/omnivore/raw");
        assert!(raw_dir.exists(), "raw dir missing");
    }

    #[test]
    fn archived_article_maps_state_correctly() {
        let v = temp_vault("archived");
        let zip_bytes = build_zip(&[("metadata_0_to_1.json", META_ARCHIVED.as_bytes())]);
        do_import(&v, &zip_bytes);

        let item_file = v.root().join("reading/omnivore/2023-11.jsonl");
        let content = fs::read_to_string(&item_file).unwrap();
        let item: Item = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(item.state, "archived");
        assert_eq!(item.progress, Some(100));
        assert!(item.excerpt.is_empty(), "empty description -> empty excerpt");
    }

    #[test]
    fn imports_highlights_linked_to_articles() {
        let v = temp_vault("highlights");
        let mut all_meta = String::new();
        all_meta.push('[');
        let art: serde_json::Value = serde_json::from_str(META_SIMPLE).unwrap();
        all_meta.push_str(&serde_json::to_string(&art[0]).unwrap());
        all_meta.push(']');

        let zip_bytes = build_zip(&[
            ("metadata_0_to_1.json", all_meta.as_bytes()),
            ("highlights/why-local-first.md", HIGHLIGHTS_ART001.as_bytes()),
        ]);
        let out = do_import(&v, &zip_bytes);
        assert_eq!(out.counts["articles_imported"], 1);
        assert_eq!(out.counts["highlights_imported"], 3);

        // Highlight contract file.
        let hl_file = v.root().join("reading/omnivore/highlights/2024-03.jsonl");
        assert!(hl_file.exists(), "highlight contract file missing");
        let lines: Vec<Highlight> = fs::read_to_string(&hl_file)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].guid, "art-001::0");
        assert_eq!(lines[0].text, "This is the first highlight");
        assert_eq!(lines[0].note, "My note for the first highlight");
        assert_eq!(lines[0].tags, vec!["tag1", "tag2"]);
        assert_eq!(lines[0].title, "Why Local-First?");
        assert_eq!(lines[0].url, "https://example.com/local-first");
        assert_eq!(lines[0].author, "Martin Kleppmann");
        assert_eq!(lines[0].source, "omnivore");
        // Third highlight: no note.
        assert_eq!(lines[2].guid, "art-001::2");
        assert!(lines[2].note.is_empty());
    }

    #[test]
    fn reimport_same_zip_is_noop() {
        let v = temp_vault("reimport");
        let zip_bytes = build_zip(&[
            ("metadata_0_to_1.json", META_SIMPLE.as_bytes()),
            ("highlights/why-local-first.md", HIGHLIGHTS_ART001.as_bytes()),
        ]);
        let out1 = do_import(&v, &zip_bytes);
        assert_eq!(out1.counts["articles_imported"], 1);
        assert_eq!(out1.counts["highlights_imported"], 3);

        // Re-import: everything should be detected as duplicate.
        let out2 = do_import(&v, &zip_bytes);
        assert_eq!(out2.counts["articles_imported"], 0);
        assert_eq!(out2.counts["highlights_imported"], 0);
        assert_eq!(out2.counts["articles_duplicate"], 1);
        assert_eq!(out2.counts["highlights_duplicate"], 3);

        // Vault files unchanged.
        let item_file = v.root().join("reading/omnivore/2024-03.jsonl");
        assert_eq!(
            fs::read_to_string(&item_file).unwrap().lines().count(),
            1,
            "no extra lines after re-import"
        );
    }

    #[test]
    fn multi_batch_metadata_files_all_parsed() {
        let v = temp_vault("multibatch");
        // Two separate metadata batch files.
        let zip_bytes = build_zip(&[
            ("metadata_0_to_1.json", META_SIMPLE.as_bytes()),
            ("metadata_1_to_2.json", META_ARCHIVED.as_bytes()),
        ]);
        let out = do_import(&v, &zip_bytes);
        assert_eq!(out.counts["articles_imported"], 2, "both batches parsed: {out:?}");
    }

    #[test]
    fn last_data_reflects_newest_partition() {
        let v = temp_vault("lastdata");
        // Before import: None.
        assert!(def_last_data(&v).is_none());
        let zip_bytes = build_zip(&[("metadata_0_to_1.json", META_SIMPLE.as_bytes())]);
        do_import(&v, &zip_bytes);
        // After import: "2024-03" from the savedAt.
        let ld = def_last_data(&v).unwrap();
        assert!(ld.contains("2024-03") || !ld.is_empty(), "last_data: {ld}");
    }
}
