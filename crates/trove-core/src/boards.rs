//! Boards: user-curated panels of generic charts, stored as markdown files
//! with YAML frontmatter under `boards/<slug>.md`.
//!
//! A board is Health's merged view (docs/roadmap.md, S7-health decision):
//! the user names each series — a catalog metric, or a table column with an
//! aggregate — so no precedence rule ever has to decide which source wins. Boards are files so they can be shared;
//! a series that recurs across shared boards is the demand signal for a
//! designed view. The body under the frontmatter is free markdown (notes on
//! what the board is for). Spec: `docs/vault-spec/boards.md`.

use std::fs;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::columns::Agg;
use crate::health::Bucket;
use crate::store::write_atomic;
use crate::vault::Vault;

pub const BOARDS_DIR: &str = "boards";

/// How a panel draws its series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum PanelKind {
    /// One or more series as lines; a day with no value breaks the line, so
    /// a data gap is visible. Series whose `unit` differs from the first's
    /// take a right-hand axis.
    Line,
    /// One series as bars.
    Bars,
    /// Two series, the second on a right-hand axis (a `line` that forces
    /// the split even when units match).
    Dual,
    /// One series as a calendar heatmap, one cell per day.
    Heatmap,
    /// The first series as bars, with a marker on every day inside the
    /// range where the second series has no value ("missing nights").
    Gaps,
    /// The latest value of each series as a card — an overview number, not
    /// a chart. `bucket`/`days` bound how far back "latest" may look.
    Tile,
}

/// One line on a panel: either a **catalog metric** (`metric`, a slug from
/// the unified health catalog, read through the typed path with its
/// source-aware semantics) or a **table column** (`table` + `column`, read
/// through the generic column index). Exactly one of the two forms.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct BoardSeries {
    /// Metric slug from the health catalog (`sleep-score`, `steps`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metric: String,
    /// With `metric`: the source to read (`oura`, `apple-health`); every
    /// source that reports it when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Table id as [`Vault::list_tables`] reports it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub table: String,
    /// Column name, or `@records` for records per day.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub column: String,
    /// How a table column folds; ignored for a metric (the catalog knows).
    #[serde(default = "default_agg")]
    pub agg: Agg,
    /// Legend label; the metric name or column name when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Divide every value by this before plotting (3600 turns seconds into
    /// hours). Presentation only; the index keeps the source's unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divide: Option<f64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
}

impl BoardSeries {
    /// A series names a metric, or a table and a column — never neither.
    pub fn is_valid(&self) -> bool {
        let metric = !self.metric.trim().is_empty();
        let table = !self.table.trim().is_empty() && !self.column.trim().is_empty();
        metric != table
    }
}

fn default_agg() -> Agg {
    Agg::Avg
}

fn default_bucket() -> Bucket {
    Bucket::Day
}

fn default_days() -> u32 {
    90
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Panel {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub kind: PanelKind,
    #[serde(default = "default_bucket")]
    pub bucket: Bucket,
    /// Days back from `to` (default 90).
    #[serde(default = "default_days")]
    pub days: u32,
    /// Last day shown, `YYYY-MM-DD`; today when absent, so a board stays
    /// live. Set it to freeze a panel around a date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub series: Vec<BoardSeries>,
}

/// The frontmatter — everything but the slug and the body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct BoardMeta {
    title: String,
    #[serde(default)]
    panels: Vec<Panel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Board {
    /// File stem: `[a-z0-9-]`, the only thing a caller may not change.
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub panels: Vec<Panel>,
    /// Markdown body under the frontmatter.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

/// Parse one board file. The frontmatter is the YAML between the leading
/// `---` line and the next; the rest is the body.
pub fn parse_board(slug: &str, text: &str) -> Result<Board> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .context("board file must start with a `---` frontmatter line")?;
    let end = rest
        .find("\n---")
        .context("board frontmatter is not closed by a `---` line")?;
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(|c| c == '-').trim_start_matches(['\r', '\n']);
    let meta: BoardMeta = serde_yaml_ng::from_str(yaml).context("board frontmatter")?;
    Ok(Board {
        slug: slug.to_string(),
        title: meta.title,
        panels: meta.panels,
        notes: body.trim_end().to_string(),
    })
}

/// Render a board back to its file text.
pub fn render_board(board: &Board) -> Result<String> {
    let meta = BoardMeta { title: board.title.clone(), panels: board.panels.clone() };
    let yaml = serde_yaml_ng::to_string(&meta).context("serializing board")?;
    let mut out = format!("---\n{yaml}---\n");
    if !board.notes.trim().is_empty() {
        out.push('\n');
        out.push_str(board.notes.trim_end());
        out.push('\n');
    }
    Ok(out)
}

/// Slugs are file stems: lowercase ASCII letters, digits, hyphens.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 80
        && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
}

/// A slug from a title: `Sleep × Calendar` → `sleep-calendar`.
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "board".into()
    } else {
        out
    }
}

impl Vault {
    /// Every board under `boards/`, by title. Files that don't parse are
    /// skipped (a half-edited board must not hide the others).
    pub fn list_boards(&self) -> Result<Vec<Board>> {
        let dir = self.resolve(BOARDS_DIR)?;
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_file() || path.extension().is_none_or(|x| x != "md") {
                continue;
            }
            let Some(slug) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
            if !valid_slug(&slug) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            if let Ok(b) = parse_board(&slug, &text) {
                out.push(b);
            }
        }
        out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()).then(a.slug.cmp(&b.slug)));
        Ok(out)
    }

    pub fn read_board(&self, slug: &str) -> Result<Board> {
        if !valid_slug(slug) {
            bail!("not a board slug: {slug:?}");
        }
        let path = self.resolve(&format!("{BOARDS_DIR}/{slug}.md"))?;
        let text = fs::read_to_string(&path).with_context(|| format!("reading board {slug}"))?;
        parse_board(slug, &text)
    }

    /// Create or replace `boards/<slug>.md`, atomically.
    pub fn write_board(&self, board: &Board) -> Result<()> {
        if !valid_slug(&board.slug) {
            bail!("not a board slug: {:?}", board.slug);
        }
        if board.title.trim().is_empty() {
            bail!("a board needs a title");
        }
        for (i, p) in board.panels.iter().enumerate() {
            if p.series.is_empty() {
                bail!("panel {} has no series", i + 1);
            }
            if p.series.len() > 6 {
                bail!("panel {} has more than six series", i + 1);
            }
            for s in &p.series {
                if !s.is_valid() {
                    bail!("panel {} has a series that is neither a metric nor a table column", i + 1);
                }
            }
        }
        let path = self.resolve(&format!("{BOARDS_DIR}/{}.md", board.slug))?;
        write_atomic(&path, render_board(board)?.as_bytes())
    }

    pub fn delete_board(&self, slug: &str) -> Result<()> {
        if !valid_slug(slug) {
            bail!("not a board slug: {slug:?}");
        }
        let path = self.resolve(&format!("{BOARDS_DIR}/{slug}.md"))?;
        fs::remove_file(&path).with_context(|| format!("deleting board {slug}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-test-boards-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn sample() -> Board {
        Board {
            slug: "sleep-and-calendar".into(),
            title: "Sleep × Calendar".into(),
            panels: vec![
                Panel {
                    title: "Sleep score vs meetings".into(),
                    kind: PanelKind::Dual,
                    bucket: Bucket::Day,
                    days: 90,
                    to: None,
                    series: vec![
                        BoardSeries {
                            metric: "sleep-score".into(),
                            source: "oura".into(),
                            label: "Sleep score".into(),
                            ..Default::default()
                        },
                        BoardSeries {
                            table: "calendar/events".into(),
                            column: "@records".into(),
                            agg: Agg::Sum,
                            label: "Events".into(),
                            ..Default::default()
                        },
                    ],
                },
                Panel {
                    title: "Hours asleep".into(),
                    kind: PanelKind::Bars,
                    bucket: Bucket::Week,
                    days: 180,
                    to: Some("2026-09-16".into()),
                    series: vec![BoardSeries {
                        table: "health/sleep/oura".into(),
                        column: "asleep_seconds".into(),
                        agg: Agg::Sum,
                        divide: Some(3600.0),
                        unit: "h".into(),
                        ..Default::default()
                    }],
                },
                Panel {
                    title: "Today".into(),
                    kind: PanelKind::Tile,
                    bucket: Bucket::Day,
                    days: 7,
                    to: None,
                    series: vec![
                        BoardSeries { metric: "readiness-score".into(), ..Default::default() },
                        BoardSeries { metric: "steps".into(), source: "apple-health".into(), ..Default::default() },
                    ],
                },
            ],
            notes: "Does a packed calendar cost sleep?".into(),
        }
    }

    #[test]
    fn board_round_trips_through_its_file_text() {
        let b = sample();
        let text = render_board(&b).unwrap();
        assert!(text.starts_with("---\ntitle: Sleep × Calendar\n"));
        assert!(text.ends_with("---\n\nDoes a packed calendar cost sleep?\n"));
        let parsed = parse_board("sleep-and-calendar", &text).unwrap();
        assert_eq!(parsed, b);
    }

    #[test]
    fn hand_written_frontmatter_uses_defaults() {
        let text = "---\ntitle: Minimal\npanels:\n  - kind: line\n    series:\n      - table: calendar/events\n        column: \"@records\"\n---\n";
        let b = parse_board("minimal", text).unwrap();
        assert_eq!(b.panels.len(), 1);
        assert_eq!(b.panels[0].bucket, Bucket::Day);
        assert_eq!(b.panels[0].days, 90);
        assert_eq!(b.panels[0].series[0].agg, Agg::Avg);
        assert_eq!(b.notes, "");
        assert!(parse_board("x", "no frontmatter").is_err());
        let metric = "---\ntitle: M\npanels:\n  - kind: tile\n    series:\n      - metric: steps\n---\n";
        let b = parse_board("m", metric).unwrap();
        assert_eq!(b.panels[0].series[0].metric, "steps");
        assert!(b.panels[0].series[0].is_valid());
        assert!(!BoardSeries { metric: "x".into(), table: "t".into(), column: "c".into(), ..Default::default() }.is_valid());
        assert!(!BoardSeries::default().is_valid());
        assert!(parse_board("x", "---\ntitle: open\n").is_err());
    }

    #[test]
    fn vault_lists_writes_and_deletes_boards() {
        let v = temp_vault("crud");
        assert!(v.list_boards().unwrap().is_empty());
        v.write_board(&sample()).unwrap();
        v.write_board(&Board { slug: "a-first".into(), title: "A first".into(), panels: vec![], notes: String::new() })
            .unwrap();
        let mut both = sample();
        both.panels[0].series[0].table = "t".into();
        both.panels[0].series[0].column = "c".into();
        assert!(v.write_board(&both).is_err());
        fs::write(v.root().join("boards/broken.md"), "not a board").unwrap();
        fs::write(v.root().join("boards/README.txt"), "ignored").unwrap();
        let titles: Vec<String> = v.list_boards().unwrap().into_iter().map(|b| b.title).collect();
        assert_eq!(titles, vec!["A first", "Sleep × Calendar"]);
        assert_eq!(v.read_board("sleep-and-calendar").unwrap(), sample());
        v.delete_board("a-first").unwrap();
        assert_eq!(v.list_boards().unwrap().len(), 1);
        assert!(v.read_board("../etc").is_err());
        assert!(v.write_board(&Board { slug: "Bad Slug".into(), title: "x".into(), panels: vec![], notes: String::new() }).is_err());
        let mut empty_series = sample();
        empty_series.panels[0].series.clear();
        assert!(v.write_board(&empty_series).is_err());
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Sleep × Calendar"), "sleep-calendar");
        assert_eq!(slugify("  Weekly!! Review "), "weekly-review");
        assert_eq!(slugify("???"), "board");
        assert!(valid_slug("sleep-1"));
        assert!(!valid_slug("-x"));
        assert!(!valid_slug("Sleep"));
        assert!(!valid_slug(""));
    }
}
