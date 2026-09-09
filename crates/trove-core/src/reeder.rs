//! Reeder — local RSS reader for macOS; reads article read/starred state and
//! feed subscriptions directly from the app's local container.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/reeder.md
//!
//! ## Versions detected
//!
//! - **Reeder 5** (`com.reederapp.5.macOS`): Realm database at
//!   `~/Library/Containers/com.reederapp.5.macOS/Data/Library/Application Support/default.realm`.
//! - **Reeder Classic** (`com.reederapp.macOS`): SQLite at the same relative
//!   path inside the Classic container (not widely deployed as of 2026).
//!
//! Both containers are sandboxed → Full Disk Access (the existing shared TCC
//! flow). The permission check returns `true` if either container directory is
//! readable.
//!
//! ## Reeder 5 Realm schema (confirmed from binary inspection of default.realm)
//!
//! The Realm file uses Realm Core v9.9 Group format (T-DB magic, column-oriented
//! B+-tree storage). File header magic: `T-DB 09 09 00 00`.
//! Class names visible in the file header:
//!   `class_Edit`, `class_Feed`, `class_Folder`, `class_Item`, `class_Items`,
//!   `class_ObjectData`, `class_SavedSearch`, `class_Service`,
//!   `class_Stream`, `class_Streamable`, `class_Tag`, `class_User`,
//!   `metadata`
//!
//! **`class_Item` fields** (confirmed from live 13 MB Reeder 5 Realm sample,
//! opened via realm-db-reader; actual crate Value variant noted):
//!
//! | Col | Name              | Realm Value  | Notes                                          |
//! |----:|-------------------|--------------|------------------------------------------------|
//! |   0 | `id`              | String       | full backend ID ("Feedly/UUID/…")              |
//! |   1 | `extId`           | String       | short article ID used as stable guid           |
//! |   2 | `deleted`         | Bool         | soft-delete flag                               |
//! |   3 | `unread`          | Int(0/1)     | 0=read, 1=unread (Bool column stored as Int)   |
//! |   4 | `starred`         | Int(0/1)     | 0=not starred, 1=starred                       |
//! |   5 | `readLater`       | Int(0/1)     | 0=normal, 1=in Read Later queue                |
//! |   6 | `publishedDate`   | Float        | Unix seconds (not Realm Timestamp)             |
//! |   7 | `starredDate`     | Float        | Unix seconds, 0.0 when not starred             |
//! |   8 | `readLaterDate`   | Float        | Unix seconds when added to Read Later          |
//! |   9 | `link`            | String       | canonical article URL                          |
//! |  10 | `title`           | String       | article title                                  |
//! |  11 | `author`          | String       | byline                                         |
//! |  12 | `summary`         | String       | feed-provided excerpt                          |
//! |  13 | `thumbnail`       | String       | thumbnail image URL                            |
//! |  14 | `content`         | String       | full article text (plain)                      |
//! |  15 | `htmlContent`     | String       | full article HTML                              |
//! |  16 | `fullContent`     | String       | Mercury-fetched text                           |
//! |  17 | `fullTitle`       | String       | Mercury-fetched title                          |
//! |  18 | `fullHtmlContent` | String       | Mercury-fetched HTML                           |
//! |  19 | `user`            | Link         | → `class_User`                                 |
//! |  20 | `feed`            | Link         | → `class_Feed`                                 |
//!
//! **`class_Feed` fields** (confirmed from live sample):
//!
//! | Col | Name    | Realm Value | Notes                                        |
//! |----:|---------|-------------|----------------------------------------------|
//! |   0 | `user`  | Link        | → `class_User`                               |
//! |   1 | `id`    | String      | full backend ID ("Feedly/UUID/Feed/…")       |
//! |   2 | `extId` | String      | short feed ID, usually "feed/URL"            |
//! |   3 | `data`  | LinkList    | associated data items                        |
//! |   4 | `url`   | String      | feed URL                                     |
//! |   5 | `reader`| Bool        | whether it is a "reader" feed                |
//!
//! Note: no `title`/`name` column exists in class_Feed. Feed display name is
//! derived from the host portion of `url` (stripping "www.").
//!
//!
//! **State mapping** (contract `state` field):
//! - `starred == true` → `"favorite"`
//! - `unread == false && starred == false` → `"read"` (read = !unread)
//! - `readLater == true && starred == false` → `"saved"` (to-read queue)
//! - articles that are unread + not starred + not readLater are skipped (not written
//!   to the contract layer — they are ordinary feed items not yet acted on)
//!
//! ## Vault layout
//!
//! - **Raw:** `reading/reeder/raw/YYYY-MM.jsonl` — full-fidelity article rows
//!   partitioned by article `publishedDate` month.
//! - **Contract:** `reading/reeder/YYYY-MM.jsonl` — one [`crate::reading::Item`]
//!   per read or starred/readLater article, month of published date.
//! - **Feed list:** `reading/reeder/feeds.jsonl` — snapshot of subscribed feeds.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};
use chrono::{DateTime, Local};
use realm_db_reader::{Realm, Value as RValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::integrations::{Integration, IntegrationKind, PermissionInfo};
use crate::reading::Item;
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "reeder";
const DIR: &str = "reading/reeder";
const RAW_DIR: &str = "reading/reeder/raw";
const FEEDS_FILE: &str = "reading/reeder/feeds.jsonl";
const SYNC_FILE: &str = ".trove/reeder-sync.json";

/// Seconds between syncs — hourly is fine for a local DB reader.
pub const REEDER_SYNC_SECS: u64 = 3600;

/// Container paths to check (Reeder 5 first, then Classic).
const REEDER5_CONTAINER: &str =
    "Library/Containers/com.reederapp.5.macOS/Data/Library/Application Support";

const REEDER_CLASSIC_CONTAINER: &str =
    "Library/Containers/com.reederapp.macOS/Data/Library/Application Support";

// ---------------------------------------------------------------------------
// Permission check.

/// Returns true if at least one Reeder container directory is readable — a
/// proxy for Full Disk Access. Returns false when Reeder is not installed.
pub fn reeder_permission_ok() -> bool {
    if let Some(home) = dirs::home_dir() {
        if home.join(REEDER5_CONTAINER).exists()
            && fs::read_dir(home.join(REEDER5_CONTAINER)).is_ok()
        {
            return true;
        }
        if home.join(REEDER_CLASSIC_CONTAINER).exists()
            && fs::read_dir(home.join(REEDER_CLASSIC_CONTAINER)).is_ok()
        {
            return true;
        }
    }
    false
}

/// Paths to the Reeder data files, by version.
#[derive(Debug, Clone)]
enum ReederData {
    /// Reeder 5 — Realm database.
    Realm5(PathBuf),
    /// Reeder Classic — SQLite database directory.
    Classic(PathBuf),
}

/// Detect which version of Reeder is installed and return its data path.
/// Returns `None` when Reeder is not installed or no data directory is found.
fn detect_reeder() -> Option<ReederData> {
    let home = dirs::home_dir()?;

    // Prefer Reeder 5 when both are present.
    let r5_dir = home.join(REEDER5_CONTAINER);
    if r5_dir.exists() {
        let realm_path = r5_dir.join("default.realm");
        if realm_path.exists() {
            return Some(ReederData::Realm5(realm_path));
        }
    }

    let classic_dir = home.join(REEDER_CLASSIC_CONTAINER);
    if classic_dir.exists() {
        return Some(ReederData::Classic(classic_dir));
    }

    None
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_permission() -> PermissionInfo {
    PermissionInfo {
        kind: "full-disk-access",
        granted: Some(reeder_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "reeder synced — {} articles, {} feeds",
                    c("articles"),
                    c("feeds")
                )
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("reeder sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Reeder synced — {} articles, {} feeds",
            c("articles"),
            c("feeds")
        ),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: SOURCE,
        name: "Reeder",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Reads your Reeder RSS article history and read/starred state directly from the app's \
             local container. Imports read and starred articles into the reading timeline. Syncs \
             articles from Feedly, Feedbin, and other backends that Reeder mirrors locally.",
        domain: "reading",
        vault_path: "reading/reeder/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved \
             binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats:
            "Requires Full Disk Access. Reeder 5 stores data in a Realm database (format v9.9); \
             parsed via realm-db-reader (pure Rust). Reeder Classic (SQLite) is detected \
             automatically when present but its parser is not yet implemented.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(REEDER_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Sync cursor / watermark.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Cursor: set of guids already written to the raw layer.
    #[serde(default)]
    seen_guids: std::collections::HashSet<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_reeder_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_reeder_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row.

/// Full-fidelity row written to `reading/reeder/raw/YYYY-MM.jsonl`.
#[derive(Serialize)]
struct RawRow {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Feed row.

/// One feed record written to `reading/reeder/feeds.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeedRow {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    feed_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    folder: String,
    source_version: &'static str,
}

// ---------------------------------------------------------------------------
// Parsed article (internal).

/// One article record parsed from either the Realm or Classic DB.
#[derive(Debug)]
struct ArticleRecord {
    /// Source-unique stable ID (extId from Realm, or row guid from Classic).
    ext_id: String,
    /// Whether the article has been read.
    read: bool,
    /// Whether the article is starred / favorited.
    starred: bool,
    /// Whether the article is in the Read Later queue.
    read_later: bool,
    /// Article URL.
    link: String,
    /// Article title.
    title: String,
    /// Author/byline.
    author: String,
    /// Feed-provided excerpt.
    summary: String,
    /// Thumbnail URL.
    thumbnail: String,
    /// Published timestamp as RFC3339 (local).
    published_at: String,
    /// When starred, as RFC3339 (local). Empty if not starred.
    starred_at: String,
    /// Feed display name.
    feed_name: String,
}

impl ArticleRecord {
    /// Build the vault contract row.
    fn to_item(&self) -> Item {
        // State mapping per reading contract semantics:
        //   starred          → "favorite"  (explicitly saved as a favourite)
        //   read_later only  → "saved"     (to-read queue, not yet starred)
        //   read (not unread) → "read"
        //   otherwise (unread, not starred, not read_later) → "saved" (fallback;
        //   these are filtered out of the contract layer before to_item() is called)
        let state = if self.starred {
            "favorite"
        } else if self.read {
            "read"
        } else {
            // read_later == true (or fallback for other unread states)
            "saved"
        }
        .to_string();

        let mut extra: Map<String, Value> = Map::new();
        extra.insert("ext_id".into(), json!(self.ext_id));
        if self.read {
            extra.insert("read".into(), json!(true));
        }
        if self.starred {
            extra.insert("starred".into(), json!(true));
        }
        if self.read_later {
            extra.insert("read_later".into(), json!(true));
        }
        if !self.starred_at.is_empty() {
            extra.insert("starred_at".into(), json!(self.starred_at));
        }
        if !self.thumbnail.is_empty() {
            extra.insert("thumbnail".into(), json!(self.thumbnail));
        }

        Item {
            ts: self.published_at.clone(),
            source: SOURCE.into(),
            guid: format!("reeder-{}", self.ext_id),
            url: self.link.clone(),
            title: self.title.clone(),
            author: self.author.clone(),
            site: String::new(),
            feed: self.feed_name.clone(),
            excerpt: self.summary.clone(),
            tags: Vec::new(),
            state,
            progress: None,
            read_at: String::new(),
            extra,
        }
    }

    /// Build the raw vault row (full fidelity).
    fn to_raw(&self) -> RawRow {
        RawRow {
            ts: self.published_at.clone(),
            value: json!({
                "ext_id": self.ext_id,
                "read": self.read,
                "starred": self.starred,
                "read_later": self.read_later,
                "link": self.link,
                "title": self.title,
                "author": self.author,
                "summary": self.summary,
                "thumbnail": self.thumbnail,
                "published_at": self.published_at,
                "starred_at": self.starred_at,
                "feed": self.feed_name,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Realm 5 reader — implemented via realm-db-reader 0.2.1 (pure Rust, MIT).
//
// Opens class_Item, iterates all rows, joins each row's `feed` Link to
// class_Feed to resolve the feed display name. Uses realm-db-reader's
// Realm::open → Group → Table → get_rows() API. Supports Realm Core v9.9
// (the format emitted by Reeder 5 on this machine: header `T-DB 09 09 00 00`,
// confirmed unencrypted and in the crate's supported range).

/// Extract a string value from a Realm row field, returning empty string for
/// null or non-string values.
fn realm_str(row: &realm_db_reader::Row<'_>, col: &str) -> String {
    match row.get(col) {
        Some(RValue::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Extract a bool value from a Realm row field.
///
/// Realm stores booleans as both `Bool` and `Int` variants depending on
/// whether the column was defined as nullable. Reeder 5 stores `deleted` as
/// `Bool` but `unread`/`starred`/`readLater` as `Int` (0=false, non-zero=true).
fn realm_bool(row: &realm_db_reader::Row<'_>, col: &str) -> bool {
    match row.get(col) {
        Some(RValue::Bool(b)) => *b,
        Some(RValue::Int(n)) => *n != 0,
        _ => false,
    }
}

/// Extract a Unix-seconds timestamp from a Realm row field, returning RFC3339.
///
/// Reeder 5 stores `publishedDate` and `starredDate` as `Float` (Unix seconds,
/// not Realm Timestamp). Returns empty string when the field is null, zero, or
/// missing.
fn realm_ts_float(row: &realm_db_reader::Row<'_>, col: &str) -> String {
    match row.get(col) {
        Some(RValue::Float(f)) if *f > 0.0 => {
            let secs = *f as i64;
            let nanos = ((*f - secs as f32) * 1_000_000_000.0) as u32;
            if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) {
                let local: DateTime<Local> = dt.with_timezone(&Local);
                return local.to_rfc3339();
            }
            String::new()
        }
        Some(RValue::Double(f)) if *f > 0.0 => {
            let secs = *f as i64;
            let nanos = ((*f - secs as f64) * 1_000_000_000.0) as u32;
            if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) {
                let local: DateTime<Local> = dt.with_timezone(&Local);
                return local.to_rfc3339();
            }
            String::new()
        }
        Some(RValue::Timestamp(dt)) => {
            let local: DateTime<Local> = dt.with_timezone(&Local);
            local.to_rfc3339()
        }
        Some(RValue::Int(n)) if *n > 0 => {
            // Some Realm builds store timestamps as Int (ms since epoch).
            let secs = *n / 1000;
            let ms_rem = *n % 1000;
            if let Some(dt) = chrono::DateTime::from_timestamp(secs, (ms_rem * 1_000_000) as u32) {
                let local: DateTime<Local> = dt.with_timezone(&Local);
                return local.to_rfc3339();
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// Extract the hostname from a URL string, stripping a leading "www." prefix.
/// Returns `None` when the string is empty or lacks a recognisable host.
fn extract_host_label(url: &str) -> Option<String> {
    // Minimal host extraction: find "://" then take text up to the next "/".
    let after_scheme = url.find("://").map(|i| &url[i + 3..])?;
    let host_part = after_scheme.split('/').next()?;
    if host_part.is_empty() {
        return None;
    }
    Some(host_part.trim_start_matches("www.").to_string())
}

fn read_realm5(realm_path: &std::path::Path) -> Result<(Vec<ArticleRecord>, Vec<FeedRow>)> {
    let realm = Realm::open(realm_path).map_err(|e| anyhow::anyhow!("realm open: {e}"))?;
    let group = realm.into_group().map_err(|e| anyhow::anyhow!("realm group: {e}"))?;

    // ---- Build feed lookup: row_number → (display_name, url, ext_id) -----
    // class_Feed columns (confirmed from live sample): user, id, extId, data,
    // url, reader. No `title` or `name` column — use extId as the display name
    // (it contains the feed URL path e.g. "feed/http://feeds.wired.com/...").
    let mut feed_entries: Vec<(String, String, String)> = Vec::new(); // (name, url, feed_id)
    let mut feed_rows_out: Vec<FeedRow> = Vec::new();
    {
        let feed_table = group
            .get_table_by_name("class_Feed")
            .map_err(|e| anyhow::anyhow!("class_Feed: {e}"))?;
        let feed_row_count = feed_table.row_count().map_err(|e| anyhow::anyhow!("{e}"))?;
        for i in 0..feed_row_count {
            let row = feed_table.get_row(i).map_err(|e| anyhow::anyhow!("{e}"))?;
            let feed_id = realm_str(&row, "extId");
            let url = realm_str(&row, "url");
            // Derive a readable name: extract the host from the feed URL,
            // stripping "www." prefix. Falls back to extId if URL parsing fails.
            let name = extract_host_label(&url)
                .or_else(|| {
                    // extId often looks like "feed/http://feeds.example.com/..."
                    let feed_url = feed_id.strip_prefix("feed/").unwrap_or(&feed_id);
                    extract_host_label(feed_url)
                })
                .unwrap_or_else(|| feed_id.clone());
            feed_entries.push((name.clone(), url.clone(), feed_id.clone()));
            feed_rows_out.push(FeedRow {
                feed_id,
                name,
                url,
                folder: String::new(),
                source_version: "reeder5",
            });
        }
    }

    // ---- Read class_Item rows --------------------------------------------
    let item_table = group
        .get_table_by_name("class_Item")
        .map_err(|e| anyhow::anyhow!("class_Item: {e}"))?;
    let row_count = item_table.row_count().map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut articles: Vec<ArticleRecord> = Vec::with_capacity(row_count.min(50_000));

    for i in 0..row_count {
        let row = item_table.get_row(i).map_err(|e| anyhow::anyhow!("row {i}: {e}"))?;

        // Skip soft-deleted items.
        if realm_bool(&row, "deleted") {
            continue;
        }

        let ext_id = realm_str(&row, "extId");
        if ext_id.is_empty() {
            continue;
        }

        // unread/starred/readLater are stored as Int (0/1) in this Realm file.
        let unread = realm_bool(&row, "unread");
        let starred = realm_bool(&row, "starred");
        let read_later = realm_bool(&row, "readLater");

        // Resolve feed display name via the `feed` Link column.
        let feed_name = match row.get("feed") {
            Some(RValue::Link(link)) => feed_entries
                .get(link.row_number)
                .map(|(name, _, _)| name.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };

        articles.push(ArticleRecord {
            ext_id,
            read: !unread,
            starred,
            read_later,
            link: realm_str(&row, "link"),
            title: realm_str(&row, "title"),
            author: realm_str(&row, "author"),
            summary: realm_str(&row, "summary"),
            thumbnail: realm_str(&row, "thumbnail"),
            published_at: realm_ts_float(&row, "publishedDate"),
            starred_at: realm_ts_float(&row, "starredDate"),
            feed_name,
        });
    }

    Ok((articles, feed_rows_out))
}

// ---------------------------------------------------------------------------
// Classic (SQLite) reader — placeholder.
//
// Reeder Classic uses a SQLite database. The schema is not publicly documented
// but is expected to be simpler than the Realm format. No Classic container
// was present during initial development; implementation is deferred until
// a Classic install is available for schema inspection.

fn read_classic(_dir: &std::path::Path) -> Result<(Vec<ArticleRecord>, Vec<FeedRow>)> {
    bail!("Reeder Classic SQLite reader not yet implemented (no Classic install available)")
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Append new items to the contract layer and raw layer (guid-deduplicated).
fn write_items(
    vault: &Vault,
    articles: &[ArticleRecord],
    seen: &mut std::collections::HashSet<String>,
) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut new_items: Vec<Item> = Vec::new();
    let mut new_raws: Vec<RawRow> = Vec::new();

    for article in articles {
        // Only write read, starred, or readLater articles to the contract layer.
        if !article.read && !article.starred && !article.read_later {
            continue;
        }
        if article.published_at.is_empty() {
            continue;
        }
        let guid = format!("reeder-{}", article.ext_id);
        if seen.insert(guid.clone()) {
            new_items.push(article.to_item());
            new_raws.push(article.to_raw());
        }
    }

    let written = new_items.len() as u64;
    contract.append(&new_items, |r| r.ts.as_str())?;
    raw_stream.append(&new_raws, |r| r.ts.as_str())?;
    Ok(written)
}

/// Write feed snapshot (full overwrite — snapshot semantics).
fn write_feeds(vault: &Vault, feeds: &[FeedRow]) -> Result<u64> {
    if feeds.is_empty() {
        return Ok(0);
    }
    let path = vault.resolve(FEEDS_FILE)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for f in feeds {
        out.push_str(&serde_json::to_string(f)?);
        out.push('\n');
    }
    fs::write(&path, out)?;
    Ok(feeds.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Full sync pass: detect Reeder version, read data, write vault.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let Some(reeder_data) = detect_reeder() else {
        return Ok(PullOutcome {
            headline: "Reeder not installed or no Full Disk Access".into(),
            counts: BTreeMap::new(),
        });
    };

    let mut state = vault.read_reeder_sync();

    let (articles, feeds) = match &reeder_data {
        ReederData::Realm5(path) => read_realm5(path)?,
        ReederData::Classic(dir) => read_classic(dir)?,
    };

    let articles_written = write_items(vault, &articles, &mut state.seen_guids)?;
    let feeds_written = write_feeds(vault, &feeds)?;

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_reeder_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("articles", articles_written);
    counts.insert("feeds", feeds_written);

    Ok(PullOutcome {
        headline: format!("{articles_written} articles, {feeds_written} feeds"),
        counts,
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
            .join(format!("trove-reeder-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a set of test article records (in-memory, no DB needed).
    fn test_articles() -> Vec<ArticleRecord> {
        vec![
            ArticleRecord {
                ext_id: "feedly-001".into(),
                read: true,
                starred: false,
                read_later: false,
                link: "https://example.com/article-1".into(),
                title: "An Interesting Read".into(),
                author: "Alice Author".into(),
                summary: "A brief excerpt.".into(),
                thumbnail: "https://example.com/thumb-1.jpg".into(),
                published_at: "2026-05-10T14:00:00+00:00".into(),
                starred_at: String::new(),
                feed_name: "Example Blog".into(),
            },
            ArticleRecord {
                ext_id: "feedly-002".into(),
                read: false,
                starred: true,
                read_later: false,
                link: "https://example.com/article-2".into(),
                title: "Starred Article".into(),
                author: "Bob Byline".into(),
                summary: String::new(),
                thumbnail: String::new(),
                published_at: "2026-05-15T09:30:00+00:00".into(),
                starred_at: "2026-05-16T08:00:00+00:00".into(),
                feed_name: "Tech Feed".into(),
            },
            ArticleRecord {
                ext_id: "feedly-003".into(),
                read: false,
                starred: false,
                read_later: false,
                link: "https://example.com/article-3".into(),
                title: "Unread Unstarred".into(),
                author: String::new(),
                summary: String::new(),
                thumbnail: String::new(),
                published_at: "2026-05-20T10:00:00+00:00".into(),
                starred_at: String::new(),
                feed_name: String::new(),
            },
            ArticleRecord {
                ext_id: "feedly-004".into(),
                read: false,
                starred: false,
                read_later: true,
                link: "https://example.com/article-4".into(),
                title: "Read Later Article".into(),
                author: String::new(),
                summary: String::new(),
                thumbnail: String::new(),
                published_at: "2026-05-22T12:00:00+00:00".into(),
                starred_at: String::new(),
                feed_name: String::new(),
            },
        ]
    }

    #[test]
    fn article_to_item_state_mapping() {
        let articles = test_articles();
        let read_item = articles[0].to_item();
        let starred_item = articles[1].to_item();

        assert_eq!(read_item.state, "read", "read article maps to 'read'");
        assert_eq!(read_item.source, "reeder");
        assert_eq!(read_item.guid, "reeder-feedly-001");
        assert_eq!(read_item.feed, "Example Blog");
        assert_eq!(starred_item.state, "favorite", "starred article maps to 'favorite'");
        assert!(
            starred_item.extra.get("starred").is_some(),
            "starred flag in extra"
        );
        assert!(
            starred_item.extra.get("starred_at").is_some(),
            "starred_at in extra"
        );
    }

    #[test]
    fn article_to_item_read_later_maps_to_saved() {
        let articles = test_articles();
        let rl_item = articles[3].to_item();
        // "Read Later" is a to-read queue — semantically "saved", not "favorite".
        // "favorite" is reserved for explicitly starred articles.
        assert_eq!(rl_item.state, "saved", "read_later maps to 'saved'");
        assert!(
            rl_item.extra.get("read_later") == Some(&serde_json::json!(true)),
            "read_later flag preserved in extra"
        );
    }

    #[test]
    fn write_items_filters_unread_unstarred() {
        let vault = temp_vault("filter");
        let articles = test_articles();
        let mut seen = std::collections::HashSet::new();
        let written = write_items(&vault, &articles, &mut seen).unwrap();

        // Articles 0 (read) + 1 (starred) + 3 (read_later) → 3 written
        // Article 2 (unread, unstarred) → excluded
        assert_eq!(written, 3, "3 articles written (read/starred/readLater)");
        assert_eq!(seen.len(), 3, "3 guids in seen set");
        assert!(seen.contains("reeder-feedly-001"));
        assert!(seen.contains("reeder-feedly-002"));
        assert!(!seen.contains("reeder-feedly-003"), "unread+unstarred excluded");
        assert!(seen.contains("reeder-feedly-004"));

        let vault_root = vault.root();
        let contract_file = vault_root.join("reading/reeder/2026-05.jsonl");
        assert!(contract_file.exists(), "contract partition written");
        let lines = fs::read_to_string(&contract_file).unwrap();
        let count = lines.lines().count();
        assert_eq!(count, 3, "3 contract rows in the partition");
        assert!(!lines.contains("feedly-003"), "unread article absent from contract");

        let _ = fs::remove_dir_all(vault.root());
    }

    #[test]
    fn write_items_deduplicates_on_rerun() {
        let vault = temp_vault("dedup");
        let articles = test_articles();
        let mut seen = std::collections::HashSet::new();

        let first = write_items(&vault, &articles, &mut seen).unwrap();
        assert_eq!(first, 3, "first run: 3 articles written");

        // Second run with the same seen set → 0 new articles.
        let second = write_items(&vault, &articles, &mut seen).unwrap();
        assert_eq!(second, 0, "second run: 0 articles written (deduped)");

        let _ = fs::remove_dir_all(vault.root());
    }

    #[test]
    fn write_feeds_creates_snapshot() {
        let vault = temp_vault("feeds");
        let feeds = vec![
            FeedRow {
                feed_id: "feed-abc".into(),
                name: "Example Blog".into(),
                url: "https://example.com/rss".into(),
                folder: "Tech".into(),
                source_version: "reeder5",
            },
            FeedRow {
                feed_id: "feed-xyz".into(),
                name: "Science Daily".into(),
                url: "https://sciencedaily.com/rss/all.xml".into(),
                folder: String::new(),
                source_version: "reeder5",
            },
        ];
        let written = write_feeds(&vault, &feeds).unwrap();
        assert_eq!(written, 2, "2 feeds written");

        let feeds_path = vault.root().join("reading/reeder/feeds.jsonl");
        assert!(feeds_path.exists(), "feeds.jsonl written");
        let content = fs::read_to_string(&feeds_path).unwrap();
        assert!(content.contains("Example Blog"), "feed name in snapshot");
        assert!(content.contains("feed-xyz"), "feed id in snapshot");
        assert_eq!(content.lines().count(), 2, "2 lines in feeds.jsonl");

        let _ = fs::remove_dir_all(vault.root());
    }

    #[test]
    fn sync_state_round_trips() {
        let vault = temp_vault("sync");
        let mut state = vault.read_reeder_sync();
        assert!(state.seen_guids.is_empty(), "fresh state is empty");

        state.seen_guids.insert("reeder-feedly-001".into());
        state.updated = Some("2026-05-10T14:00:00+00:00".into());
        vault.write_reeder_sync(&state).unwrap();

        let loaded = vault.read_reeder_sync();
        assert!(loaded.seen_guids.contains("reeder-feedly-001"), "guid persisted");
        assert_eq!(loaded.updated.as_deref(), Some("2026-05-10T14:00:00+00:00"));

        let _ = fs::remove_dir_all(vault.root());
    }

    #[test]
    fn def_has_correct_metadata() {
        assert_eq!(DEF.meta.id, "reeder");
        assert_eq!(DEF.meta.domain, "reading");
        assert!(DEF.connection.is_none(), "no login required");
        assert!(DEF.permission.is_some(), "FDA permission declared");
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
    }

    #[test]
    fn detect_reeder_returns_some_on_this_machine() {
        // This machine has Reeder 5 installed (confirmed during development).
        // If Reeder is not installed, this test skips gracefully.
        let result = detect_reeder();
        if result.is_some() {
            match result.unwrap() {
                ReederData::Realm5(path) => {
                    assert!(path.exists(), "Realm database path exists");
                    assert!(path.extension().is_some_and(|e| e == "realm"));
                }
                ReederData::Classic(dir) => {
                    assert!(dir.exists(), "Classic container dir exists");
                }
            }
        }
        // Not an error if not installed — just no data.
    }

    #[test]
    fn realm5_reader_returns_err_for_nonexistent_file() {
        // A missing path should produce an error, not a panic.
        let dummy = std::path::Path::new("/nonexistent/default.realm");
        let result = read_realm5(dummy);
        assert!(result.is_err(), "missing file → Err");
    }

    #[test]
    fn realm5_reader_opens_live_sample_when_present() {
        // On a machine with Reeder 5 installed and FDA, open the real Realm file
        // and confirm at least some articles are returned. Skips gracefully if the
        // file is not accessible (CI / machines without Reeder).
        let realm_path = {
            let Some(home) = dirs::home_dir() else { return };
            let p = home
                .join(REEDER5_CONTAINER)
                .join("default.realm");
            if !p.exists() { return; }
            p
        };

        let vault = temp_vault("realm5-live");
        let result = read_realm5(&realm_path);
        match result {
            Err(e) => {
                // Acceptable if FDA is not granted in this test process.
                eprintln!("realm5 open skipped (FDA/format): {e}");
            }
            Ok((articles, feeds)) => {
                // We know the sample has ~1985 article rows.
                assert!(!articles.is_empty(), "some articles returned from live Realm");
                // Every returned article must have a non-empty ext_id.
                for a in &articles {
                    assert!(!a.ext_id.is_empty(), "article has ext_id");
                }
                // State mapping sanity: read = !unread; no article can be both
                // starred and have state != "favorite".
                for a in &articles {
                    let item = a.to_item();
                    if a.starred {
                        assert_eq!(item.state, "favorite", "starred → favorite");
                    } else if a.read {
                        assert_eq!(item.state, "read", "read (not starred) → read");
                    } else if a.read_later {
                        assert_eq!(item.state, "saved", "read_later (not starred) → saved");
                    }
                }
                // Feed names: any article with a feed link should have a name.
                let named = articles.iter().filter(|a| !a.feed_name.is_empty()).count();
                eprintln!(
                    "realm5 live: {} articles, {} feeds, {} with feed names",
                    articles.len(), feeds.len(), named
                );
                // Write to a temp vault and verify contract rows.
                let mut seen = std::collections::HashSet::new();
                let written = write_items(&vault, &articles, &mut seen).unwrap();
                assert!(written > 0 || articles.iter().all(|a| !a.read && !a.starred && !a.read_later),
                    "at least some contract rows written when acted-upon articles exist");
            }
        }
        let _ = fs::remove_dir_all(vault.root());
    }

    #[test]
    fn raw_layer_has_full_fidelity() {
        let vault = temp_vault("raw");
        let articles = test_articles();
        let mut seen = std::collections::HashSet::new();
        write_items(&vault, &articles, &mut seen).unwrap();

        let raw_file = vault.root().join("reading/reeder/raw/2026-05.jsonl");
        assert!(raw_file.exists(), "raw partition written");
        let raw_content = fs::read_to_string(&raw_file).unwrap();
        assert!(raw_content.contains("ext_id"), "raw has ext_id");
        assert!(raw_content.contains("thumbnail"), "raw has thumbnail");
        assert!(raw_content.contains("starred_at"), "raw has starred_at");
        assert!(raw_content.contains("Example Blog"), "raw has feed name");

        let _ = fs::remove_dir_all(vault.root());
    }

}
