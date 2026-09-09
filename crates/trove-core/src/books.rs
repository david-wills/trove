//! Apple Books snapshot + diff collector: highlights, notes, reading progress.
//!
//! Books splits its state across two FDA-gated SQLite DBs in the
//! `com.apple.iBooksX` container (measured, see `docs/notes/books-schema.md`):
//! annotations in `AEAnnotation/AEAnnotation_*.sqlite`, the library catalog +
//! reading progress in `BKLibrary/BKLibrary-*.sqlite`. **Filenames carry
//! version stamps — discover by glob, never hardcode.** Neither DB keeps
//! history, so this is snapshot+diff in the [`crate::podcasts`] /
//! [`crate::music_library`] mold:
//!
//! - `books/annotations.jsonl` — every non-deleted text annotation
//!   (`ZANNOTATIONDELETED=0`, selected text present), atomically rewritten.
//! - `books/library.jsonl` — every library asset (title, author, reading
//!   progress, finished state), atomically rewritten.
//! - `books/events/YYYY-MM.jsonl` — append-only diffs: `highlighted`,
//!   `note_changed`, `removed` (highlight soft-deleted or gone), `progress`
//!   (reading progress advanced), `finished`.
//!
//! **The first-ever snapshot emits no events** — baseline only.
//!
//! `highlighted` events carry the row's own creation date — a highlight made
//! between snapshots keeps its true timestamp, the event `ts` only records
//! when the snapshot noticed it. Timestamps in both DBs are Apple epoch
//! (seconds since 2001-01-01 UTC), converted to RFC3339 local at read.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

const ANNOTATIONS_FILE: &str = "books/annotations.jsonl";

fn def_collect(
    vault: &Vault,
    now: chrono::DateTime<chrono::Local>,
) -> Result<crate::registry::CollectOutcome> {
    let stats = read_books_via_copy()
        .and_then(|(anns, assets)| vault.books_snapshot(&anns, &assets, &now.to_rfc3339()))?;
    Ok(crate::registry::CollectOutcome::note(format!(
        "books snapshot: {stats:?}"
    )))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    use crate::registry::readable;
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(find_books_dbs().is_some_and(|(a, b)| readable(&a) && readable(&b))),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    let root = vault.root();
    [
        crate::registry::file_mtime(&root.join(ANNOTATIONS_FILE)),
        crate::registry::file_mtime(&root.join(LIBRARY_FILE)),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "books",
        name: "Apple Books",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Daily snapshot of highlights, notes, and reading progress from Apple Books.",
        domain: "media",
        vault_path: "books/",
        toggleable: true,
        setup: &["Uses the same Full Disk Access grant as Messages and Safari — nothing extra once that's done."],
        caveats: "Apple Books only — Kindle highlights are a separate, planned source. Highlights keep their true creation dates even when made between snapshots.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::daily(crate::browser::BROWSER_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};
const LIBRARY_FILE: &str = "books/library.jsonl";

const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

/// Reading progress must advance this much (fraction of the book) to count
/// as a `progress` event — below it, re-opens and scroll jitter are noise.
const PROGRESS_MIN_FRACTION: f64 = 0.05;

/// One highlight/note — a line in `books/annotations.jsonl`. Only `uuid` is
/// required; everything else defaults so old lines keep deserializing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookAnnotation {
    /// `ZANNOTATIONUUID` — stable unique key.
    pub uuid: String,
    /// `ZANNOTATIONASSETID` — joins to [`BookAsset::asset_id`].
    #[serde(default)]
    pub asset_id: String,
    /// Book title/author, denormalized from the library DB at read time
    /// (empty when the asset is no longer in the library).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    /// The highlighted passage.
    #[serde(default)]
    pub text: String,
    /// The user's own note, when they wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Raw `ZANNOTATIONSTYLE` (highlight color, 0–5).
    #[serde(default)]
    pub style: i64,
    #[serde(default)]
    pub underline: bool,
    /// Opaque position (epubcfi) within the book.
    #[serde(default)]
    pub location: String,
    /// RFC3339 local creation/modification times from the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// One library asset — a line in `books/library.jsonl`. Only `asset_id` is
/// required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookAsset {
    /// `ZASSETID`.
    pub asset_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub genre: String,
    /// `ZREADINGPROGRESS`, 0.0–1.0.
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub is_finished: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_finished: Option<String>,
}

/// One line of `books/events/YYYY-MM.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookEvent {
    /// RFC3339 local time of the snapshot that observed the change.
    pub ts: String,
    /// "highlighted", "note_changed", "removed", "progress", or "finished".
    pub kind: String,
    /// Annotation uuid for highlight events, asset_id for progress/finished.
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    /// The highlighted passage, on `highlighted` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The note text, on `highlighted`/`note_changed` events that have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The annotation's own creation time, on `highlighted` events — the
    /// real highlight time, independent of when the snapshot ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// New reading progress, on `progress`/`finished` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
}

/// Result of one snapshot pass, for logging/status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BooksSnapshotStats {
    pub annotations: u64,
    pub assets: u64,
    pub highlighted: u64,
    pub note_changed: u64,
    pub removed: u64,
    pub progress: u64,
    pub finished: u64,
    /// True when this was the first-ever snapshot (baseline, no events).
    pub baseline: bool,
}

/// The pure annotation diff. The reader only returns non-deleted rows, so a
/// soft-deleted highlight simply disappears → `removed`.
pub fn diff_annotations(
    old: &[BookAnnotation],
    new: &[BookAnnotation],
    ts: &str,
) -> Vec<BookEvent> {
    let old_by_id: BTreeMap<&str, &BookAnnotation> =
        old.iter().map(|a| (a.uuid.as_str(), a)).collect();
    let new_ids: HashSet<&str> = new.iter().map(|a| a.uuid.as_str()).collect();
    let mut events = Vec::new();
    for ann in new {
        match old_by_id.get(ann.uuid.as_str()) {
            None => events.push(BookEvent {
                ts: ts.to_string(),
                kind: "highlighted".into(),
                id: ann.uuid.clone(),
                title: ann.title.clone(),
                author: ann.author.clone(),
                text: Some(ann.text.clone()),
                note: ann.note.clone(),
                created: ann.created.clone(),
                progress: None,
            }),
            Some(before) if before.note != ann.note => events.push(BookEvent {
                ts: ts.to_string(),
                kind: "note_changed".into(),
                id: ann.uuid.clone(),
                title: ann.title.clone(),
                author: ann.author.clone(),
                text: None,
                note: ann.note.clone(),
                created: None,
                progress: None,
            }),
            Some(_) => {}
        }
    }
    for ann in old {
        if !new_ids.contains(ann.uuid.as_str()) {
            events.push(BookEvent {
                ts: ts.to_string(),
                kind: "removed".into(),
                id: ann.uuid.clone(),
                title: ann.title.clone(),
                author: ann.author.clone(),
                text: Some(ann.text.clone()),
                note: None,
                created: None,
                progress: None,
            });
        }
    }
    events
}

/// The pure asset diff: at most one event per asset, `finished` beats
/// `progress`.
pub fn diff_assets(old: &[BookAsset], new: &[BookAsset], ts: &str) -> Vec<BookEvent> {
    let old_by_id: BTreeMap<&str, &BookAsset> =
        old.iter().map(|a| (a.asset_id.as_str(), a)).collect();
    let mut events = Vec::new();
    for asset in new {
        let Some(before) = old_by_id.get(asset.asset_id.as_str()) else {
            continue; // new assets are state, not events — same as baseline
        };
        let finished_now = asset.is_finished && !before.is_finished
            || asset.date_finished.is_some() && before.date_finished.is_none();
        let kind = if finished_now {
            "finished"
        } else if asset.progress >= before.progress + PROGRESS_MIN_FRACTION {
            "progress"
        } else {
            continue;
        };
        events.push(BookEvent {
            ts: ts.to_string(),
            kind: kind.into(),
            id: asset.asset_id.clone(),
            title: asset.title.clone(),
            author: asset.author.clone(),
            text: None,
            note: None,
            created: None,
            progress: Some(asset.progress),
        });
    }
    events
}

impl Vault {
    pub fn load_book_annotations(&self) -> Result<Vec<BookAnnotation>> {
        self.load_books_jsonl(ANNOTATIONS_FILE)
    }

    pub fn load_book_library(&self) -> Result<Vec<BookAsset>> {
        self.load_books_jsonl(LIBRARY_FILE)
    }

    fn load_books_jsonl<T: serde::de::DeserializeOwned>(&self, rel: &str) -> Result<Vec<T>> {
        let path = self.resolve(rel)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<T>(l).ok())
            .collect())
    }

    /// One snapshot pass over both stores: diff, append events, atomically
    /// rewrite both snapshots. The first-ever snapshot (no annotations file)
    /// writes baselines and emits no events. `ts` is the injected RFC3339
    /// local snapshot time.
    pub fn books_snapshot(
        &self,
        annotations: &[BookAnnotation],
        assets: &[BookAsset],
        ts: &str,
    ) -> Result<BooksSnapshotStats> {
        let baseline = !self.resolve(ANNOTATIONS_FILE)?.exists();
        let events = if baseline {
            Vec::new()
        } else {
            let mut ev = diff_annotations(&self.load_book_annotations()?, annotations, ts);
            ev.extend(diff_assets(&self.load_book_library()?, assets, ts));
            ev
        };
        self.append_book_events(&events)?;
        self.write_snapshot(ANNOTATIONS_FILE, annotations)?;
        self.write_snapshot(LIBRARY_FILE, assets)?;
        let count = |kind: &str| events.iter().filter(|e| e.kind == kind).count() as u64;
        Ok(BooksSnapshotStats {
            annotations: annotations.len() as u64,
            assets: assets.len() as u64,
            highlighted: count("highlighted"),
            note_changed: count("note_changed"),
            removed: count("removed"),
            progress: count("progress"),
            finished: count("finished"),
            baseline,
        })
    }

    /// Book events over an inclusive date range, chronological.
    pub fn books_events(&self, from: &str, to: &str) -> Result<Vec<BookEvent>> {
        let (from_month, to_month) = (&from[..7.min(from.len())], &to[..7.min(to.len())]);
        let dir = self.root().join("books/events");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(month) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if month < from_month || month > to_month {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            out.extend(
                body.lines()
                    .filter_map(|l| serde_json::from_str::<BookEvent>(l).ok())
                    .filter(|e| {
                        let day = &e.ts[..10.min(e.ts.len())];
                        day >= from && day <= to
                    }),
            );
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(out)
    }

    fn append_book_events(&self, events: &[BookEvent]) -> Result<()> {
        self.stream("books/events", crate::store::Partition::Month).append(events, |e| &e.ts)
    }
}

/// Locate the live DBs inside the iBooksX container by glob — the filenames
/// carry version stamps (`AEAnnotation_v10312011_1727_local.sqlite`,
/// `BKLibrary-1-091020131601.sqlite`) and may change across macOS versions.
pub fn find_books_dbs() -> Option<(PathBuf, PathBuf)> {
    let docs = PathBuf::from(std::env::var_os("HOME")?)
        .join("Library/Containers/com.apple.iBooksX/Data/Documents");
    let ann = newest_sqlite(&docs.join("AEAnnotation"), "AEAnnotation")?;
    let lib = newest_sqlite(&docs.join("BKLibrary"), "BKLibrary")?;
    Some((ann, lib))
}

fn newest_sqlite(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|x| x.to_str()) == Some("sqlite")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Read both live DBs via copy-then-read (Books holds WAL locks). Returns
/// (annotations, assets) with titles/authors denormalized onto annotations.
pub fn read_books_via_copy() -> Result<(Vec<BookAnnotation>, Vec<BookAsset>)> {
    let (ann_db, lib_db) =
        find_books_dbs().context("Books DBs not found under com.apple.iBooksX")?;
    let assets = with_db_copy(&lib_db, "trove-books-lib", read_book_library_db)?;
    let mut annotations = with_db_copy(&ann_db, "trove-books-ann", read_annotations_db)?;
    let by_id: BTreeMap<&str, &BookAsset> =
        assets.iter().map(|a| (a.asset_id.as_str(), a)).collect();
    for ann in &mut annotations {
        if let Some(asset) = by_id.get(ann.asset_id.as_str()) {
            ann.title = asset.title.clone();
            ann.author = asset.author.clone();
        }
    }
    Ok((annotations, assets))
}

fn with_db_copy<T>(db: &Path, stem: &str, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let stem = format!("{stem}-{}", std::process::id());
    let tmp = std::env::temp_dir().join(format!("{stem}.sqlite"));
    let cleanup = |tmp: &Path| {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", tmp.display())));
        }
    };
    cleanup(&tmp);
    fs::copy(db, &tmp).with_context(|| format!("copying {}", db.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            let _ = fs::copy(&side, PathBuf::from(format!("{}{suffix}", tmp.display())));
        }
    }
    let result = f(&tmp);
    cleanup(&tmp);
    result
}

/// Read non-deleted text annotations out of an AEAnnotation DB (or copy, or
/// fixture). Titles/authors are left empty — the caller joins them from the
/// library DB.
pub fn read_annotations_db(db_path: &Path) -> Result<Vec<BookAnnotation>> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("opening books annotation copy {}", db_path.display()))?;
    let mut stmt = conn.prepare(
        "SELECT ZANNOTATIONUUID, ZANNOTATIONASSETID, ZANNOTATIONSELECTEDTEXT,
                ZANNOTATIONNOTE, ZANNOTATIONSTYLE, ZANNOTATIONISUNDERLINE,
                ZANNOTATIONLOCATION, ZANNOTATIONCREATIONDATE,
                ZANNOTATIONMODIFICATIONDATE
         FROM ZAEANNOTATION
         WHERE ZANNOTATIONDELETED = 0 AND ZANNOTATIONSELECTEDTEXT IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(BookAnnotation {
            uuid: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            asset_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            title: String::new(),
            author: String::new(),
            text: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            note: row.get::<_, Option<String>>(3)?,
            style: row.get::<_, Option<i64>>(4)?.unwrap_or_default(),
            underline: row.get::<_, Option<i64>>(5)?.unwrap_or_default() != 0,
            location: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            created: row
                .get::<_, Option<f64>>(7)?
                .and_then(apple_epoch_to_rfc3339),
            modified: row
                .get::<_, Option<f64>>(8)?
                .and_then(apple_epoch_to_rfc3339),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let ann = row?;
        if !ann.uuid.is_empty() {
            out.push(ann);
        }
    }
    Ok(out)
}

/// Read the library catalog out of a BKLibrary DB (or copy, or fixture).
pub fn read_book_library_db(db_path: &Path) -> Result<Vec<BookAsset>> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("opening books library copy {}", db_path.display()))?;
    let mut stmt = conn.prepare(
        "SELECT ZASSETID, ZTITLE, ZAUTHOR, ZGENRE, ZREADINGPROGRESS,
                ZISFINISHED, ZLASTOPENDATE, ZDATEFINISHED
         FROM ZBKLIBRARYASSET",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(BookAsset {
            asset_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            author: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            genre: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            progress: row.get::<_, Option<f64>>(4)?.unwrap_or_default(),
            is_finished: row.get::<_, Option<i64>>(5)?.unwrap_or_default() != 0,
            last_opened: row
                .get::<_, Option<f64>>(6)?
                .and_then(apple_epoch_to_rfc3339),
            date_finished: row
                .get::<_, Option<f64>>(7)?
                .and_then(apple_epoch_to_rfc3339),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let asset = row?;
        if !asset.asset_id.is_empty() {
            out.push(asset);
        }
    }
    Ok(out)
}

fn apple_epoch_to_rfc3339(secs: f64) -> Option<String> {
    if secs <= 0.0 {
        return None;
    }
    chrono::DateTime::from_timestamp(secs as i64 + APPLE_EPOCH_OFFSET, 0)
        .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-books-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn ann(uuid: &str, text: &str) -> BookAnnotation {
        BookAnnotation {
            uuid: uuid.into(),
            asset_id: "ASSET-1".into(),
            title: "The Alchemist Cocktail Book".into(),
            author: "The Alchemist".into(),
            text: text.into(),
            note: None,
            style: 3,
            underline: false,
            location: "epubcfi(/6/24!/4/2)".into(),
            created: Some("2025-12-15T05:50:36-08:00".into()),
            modified: None,
        }
    }

    fn asset(id: &str, progress: f64) -> BookAsset {
        BookAsset {
            asset_id: id.into(),
            title: "Death & Co".into(),
            author: "David Kaplan".into(),
            genre: "Cocktails".into(),
            progress,
            is_finished: false,
            last_opened: Some("2025-12-22T17:37:23-08:00".into()),
            date_finished: None,
        }
    }

    const TS: &str = "2026-06-11T05:00:00-07:00";

    #[test]
    fn new_highlight_carries_its_own_creation_date() {
        let old = vec![ann("A", "old one")];
        let new = vec![ann("A", "old one"), ann("B", "30ml gin")];
        let events = diff_annotations(&old, &new, TS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "highlighted");
        assert_eq!(events[0].id, "B");
        assert_eq!(events[0].text.as_deref(), Some("30ml gin"));
        assert_eq!(events[0].created.as_deref(), Some("2025-12-15T05:50:36-08:00"));
        assert_eq!(events[0].ts, TS, "event ts is the snapshot time");
    }

    #[test]
    fn note_change_and_removal() {
        let mut noted = ann("A", "passage");
        noted.note = Some("try this".into());
        let events = diff_annotations(&[ann("A", "passage"), ann("B", "doomed")], &[noted], TS);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "note_changed");
        assert_eq!(events[0].note.as_deref(), Some("try this"));
        assert_eq!(events[1].kind, "removed");
        assert_eq!(events[1].id, "B");

        // Unchanged annotations are silent.
        let same = vec![ann("A", "passage")];
        assert!(diff_annotations(&same, &same.clone(), TS).is_empty());
    }

    #[test]
    fn asset_progress_and_finished() {
        // Jitter below the threshold is silent.
        assert!(diff_assets(&[asset("X", 0.50)], &[asset("X", 0.54)], TS).is_empty());

        let events = diff_assets(&[asset("X", 0.50)], &[asset("X", 0.60)], TS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "progress");
        assert_eq!(events[0].progress, Some(0.60));

        // Finishing wins over progress — one event.
        let mut done = asset("X", 1.0);
        done.is_finished = true;
        done.date_finished = Some(TS.into());
        let events = diff_assets(&[asset("X", 0.50)], &[done], TS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "finished");

        // New assets are state, not events.
        assert!(diff_assets(&[], &[asset("Y", 0.0)], TS).is_empty());
    }

    /// Byte-parity contract for all three write paths (snapshot rewrites +
    /// month-keyed event append): exact file bytes, pinned before the port
    /// onto `store` and unchanged by it.
    #[test]
    fn snapshot_and_events_write_byte_identical_files() {
        let v = temp_vault("parity");
        v.books_snapshot(&[ann("A", "one")], &[asset("X", 0.5)], "2026-06-10T05:00:00-07:00")
            .unwrap();
        // Second pass: a new highlight; third pass: a progress jump — two
        // event lines appended to the same month file across calls.
        v.books_snapshot(&[ann("A", "one"), ann("B", "30ml gin")], &[asset("X", 0.5)], TS)
            .unwrap();
        v.books_snapshot(
            &[ann("A", "one"), ann("B", "30ml gin")],
            &[asset("X", 0.6)],
            "2026-06-12T05:00:00-07:00",
        )
        .unwrap();

        let ann_a = "{\"uuid\":\"A\",\"asset_id\":\"ASSET-1\",\"title\":\"The Alchemist Cocktail Book\",\"author\":\"The Alchemist\",\"text\":\"one\",\"style\":3,\"underline\":false,\"location\":\"epubcfi(/6/24!/4/2)\",\"created\":\"2025-12-15T05:50:36-08:00\"}";
        let ann_b = ann_a.replacen("\"A\"", "\"B\"", 1).replacen("\"one\"", "\"30ml gin\"", 1);
        assert_eq!(
            fs::read_to_string(v.root().join("books/annotations.jsonl")).unwrap(),
            format!("{ann_a}\n{ann_b}\n"),
            "annotations snapshot rewrite"
        );
        assert_eq!(
            fs::read_to_string(v.root().join("books/library.jsonl")).unwrap(),
            "{\"asset_id\":\"X\",\"title\":\"Death & Co\",\"author\":\"David Kaplan\",\"genre\":\"Cocktails\",\"progress\":0.6,\"is_finished\":false,\"last_opened\":\"2025-12-22T17:37:23-08:00\"}\n",
            "library snapshot rewrite"
        );
        assert_eq!(
            fs::read_to_string(v.root().join("books/events/2026-06.jsonl")).unwrap(),
            "{\"ts\":\"2026-06-11T05:00:00-07:00\",\"kind\":\"highlighted\",\"id\":\"B\",\"title\":\"The Alchemist Cocktail Book\",\"author\":\"The Alchemist\",\"text\":\"30ml gin\",\"created\":\"2025-12-15T05:50:36-08:00\"}\n\
             {\"ts\":\"2026-06-12T05:00:00-07:00\",\"kind\":\"progress\",\"id\":\"X\",\"title\":\"Death & Co\",\"author\":\"David Kaplan\",\"progress\":0.6}\n",
            "month-keyed event append extends across calls"
        );
        assert!(!v.root().join("books/annotations.jsonl.tmp").exists());
    }

    #[test]
    fn first_snapshot_is_a_silent_baseline_then_diffs() {
        let v = temp_vault("baseline");
        let stats = v
            .books_snapshot(&[ann("A", "one")], &[asset("X", 0.5)], "2026-06-10T05:00:00-07:00")
            .unwrap();
        assert!(stats.baseline);
        assert_eq!((stats.annotations, stats.assets), (1, 1));
        assert_eq!(stats.highlighted + stats.progress, 0);
        assert!(!v.root().join("books/events").exists());

        let stats = v
            .books_snapshot(&[ann("A", "one"), ann("B", "two")], &[asset("X", 0.7)], TS)
            .unwrap();
        assert!(!stats.baseline);
        assert_eq!((stats.highlighted, stats.progress), (1, 1));
        assert_eq!(v.load_book_annotations().unwrap().len(), 2);
        assert_eq!(v.load_book_library().unwrap()[0].progress, 0.7);

        let events = v.books_events("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "highlighted");
        assert_eq!(events[1].kind, "progress");
    }

    #[test]
    fn minimal_old_style_lines_still_parse() {
        let a: BookAnnotation = serde_json::from_str(r#"{"uuid":"U-1"}"#).unwrap();
        assert_eq!(a.uuid, "U-1");
        assert!(a.note.is_none() && a.created.is_none());
        let body = serde_json::to_string(&a).unwrap();
        assert!(!body.contains("note") && !body.contains("created"));

        let b: BookAsset = serde_json::from_str(r#"{"asset_id":"X"}"#).unwrap();
        assert_eq!(b.asset_id, "X");
        assert_eq!(b.progress, 0.0);
        assert!(!b.is_finished);
    }

    #[test]
    fn readers_filter_convert_and_glob() {
        let dir =
            std::env::temp_dir().join(format!("trove-books-fixture-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let ann_db = dir.join("AEAnnotation_v1_local.sqlite");
        let conn = rusqlite::Connection::open(&ann_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZAEANNOTATION (Z_PK INTEGER PRIMARY KEY,
                 ZANNOTATIONUUID TEXT, ZANNOTATIONASSETID TEXT,
                 ZANNOTATIONSELECTEDTEXT TEXT, ZANNOTATIONNOTE TEXT,
                 ZANNOTATIONSTYLE INTEGER, ZANNOTATIONISUNDERLINE INTEGER,
                 ZANNOTATIONLOCATION TEXT, ZANNOTATIONCREATIONDATE REAL,
                 ZANNOTATIONMODIFICATIONDATE REAL, ZANNOTATIONDELETED INTEGER);
             -- Real highlight, created 2025-12-15T13:50:36Z = Apple 787499436.
             INSERT INTO ZAEANNOTATION VALUES
                 (1, 'U-KEEP', 'ASSET-1', '30ml gin', 'tasty', 3, 0,
                  'epubcfi(/6/24)', 787499436.0, NULL, 0);
             -- Soft-deleted: filtered out.
             INSERT INTO ZAEANNOTATION VALUES
                 (2, 'U-DEL', 'ASSET-1', 'gone', NULL, 3, 0, '', 787499436.0, NULL, 1);
             -- Bookmark (no text): filtered out.
             INSERT INTO ZAEANNOTATION VALUES
                 (3, 'U-MARK', 'ASSET-1', NULL, NULL, 0, 0, '', 787499436.0, NULL, 0);",
        )
        .unwrap();
        drop(conn);

        let lib_db = dir.join("BKLibrary-1-test.sqlite");
        let conn = rusqlite::Connection::open(&lib_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZBKLIBRARYASSET (Z_PK INTEGER PRIMARY KEY,
                 ZASSETID TEXT, ZTITLE TEXT, ZAUTHOR TEXT, ZGENRE TEXT,
                 ZREADINGPROGRESS REAL, ZISFINISHED INTEGER,
                 ZLASTOPENDATE REAL, ZDATEFINISHED REAL);
             INSERT INTO ZBKLIBRARYASSET VALUES
                 (1, 'ASSET-1', 'The Alchemist Cocktail Book', 'The Alchemist',
                  'Cocktails', 0.405, 0, 787499436.0, NULL);",
        )
        .unwrap();
        drop(conn);

        let anns = read_annotations_db(&ann_db).unwrap();
        assert_eq!(anns.len(), 1, "deleted + textless rows filtered");
        assert_eq!(anns[0].uuid, "U-KEEP");
        assert_eq!(anns[0].note.as_deref(), Some("tasty"));
        let created = anns[0].created.as_deref().unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(created).unwrap();
        assert_eq!(parsed.timestamp(), 787499436 + APPLE_EPOCH_OFFSET);

        let assets = read_book_library_db(&lib_db).unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].title, "The Alchemist Cocktail Book");
        assert!((assets[0].progress - 0.405).abs() < 1e-9);

        // Glob discovery prefers the highest-sorting (newest-stamped) file.
        let newer = dir.join("AEAnnotation_v2_local.sqlite");
        fs::write(&newer, b"").unwrap();
        assert_eq!(newest_sqlite(&dir, "AEAnnotation").unwrap(), newer);
        assert_eq!(newest_sqlite(&dir, "BKLibrary").unwrap(), lib_db);
        assert!(newest_sqlite(&dir, "Nope").is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
