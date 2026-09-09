//! Google Play Books collector — bookshelves (Have Read / Reading Now /
//! To Read / Favorites) and Play Books annotations (highlights/notes) from
//! every connected Google account, into the new `books/google/` store — the
//! cloud sibling of [`crate::books`] (Apple Books). The OAuth side (connect,
//! token store, refresh, reconnect flagging) is owned by
//! [`crate::sync::google`]; this module only asks it for a fresh token per
//! account via [`crate::sync::google::fresh_token`]. See [`crate::gmail`]
//! for the worked per-account-pull template.
//!
//! **On-disk design** (the snapshot+events pattern, like the Apple sibling):
//!
//! - `books/google/library.jsonl` — snapshot, atomically rewritten: one row
//!   per (account, shelf, volume), stably sorted. Shelf *membership* is the
//!   library state.
//! - `books/google/events/YYYY-MM.jsonl` — append-only diffs of that
//!   membership: `added`/`removed` per shelf. A book moving To Read →
//!   Have Read is reading history — complete data, squarely in scope — and
//!   shows up as a `removed` on one shelf plus an `added` on the other.
//! - `books/google/annotations/YYYY-MM.jsonl` — append-only highlights and
//!   notes, partitioned by their own updated/created month (a highlight made
//!   between passes keeps its true timestamp), deduped by annotation id
//!   across passes.
//! - `books/google/index.md` — the human-readable summary: per account,
//!   shelves with counts, annotation count, errors.
//!
//! **Baselines are per account**: the first pass that sees an account writes
//! its shelf rows silently (no `added` flood) — exactly like the Apple
//! sibling's first snapshot, and it also makes connecting a *second* account
//! quiet while existing accounts keep diffing.
//!
//! **Books API quirks this module absorbs:**
//!
//! - The built-in my-library shelves have fixed numeric ids (Favorites=0,
//!   To Read=2, Reading Now=3, Have Read=4, Reviewed=5) plus user-created
//!   custom shelves; we always use the listed titles, never hardcode names.
//! - Individual shelves can 404/403 when empty or restricted. A failed shelf
//!   is **soft**: its error is recorded, its old snapshot rows are carried
//!   forward unchanged (so a transient failure never fabricates `removed`
//!   events), and the rest of the pass continues.
//! - The annotations endpoint is an older API surface that some accounts /
//!   client configurations 403 on. Annotation failures are soft too — they
//!   must never fail the shelves pull.
//! - `totalItems` on shelf-volume pages can overcount; paging also stops on
//!   the first empty page so an inflated total can't loop forever.
//! - Annotations support `updatedMin` for incremental pulls; the per-account
//!   high-water mark is the max raw `updated` seen. The boundary record can
//!   be re-returned next pass — the id dedupe absorbs the overlap, and a
//!   lost cursor only costs a re-list (never duplicate rows), per the vault
//!   cursor conventions.
//!
//! Per-account failures are recorded in `.trove/google-books-sync.json`
//! (atomic writes) and never abort other accounts. Data rows are keyed by
//! the account *email* (the mbox `account` pattern); sync state is keyed by
//! the stable Google `sub`. Rows for a disconnected account stay in the
//! snapshot — vault data stays complete; only the sync state is pruned.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

/// Seconds between Play Books passes in the watcher loop. Library churn is
/// slow (people shelve a few books a week, not a minute), so hourly is
/// plenty; `every_on_run` so re-enabling the toggle fires within one poll.
pub const GOOGLE_BOOKS_SYNC_SECS: u64 = 3600;

fn def_collect(
    vault: &Vault,
    now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_google_books(now)?;
    Ok(crate::registry::CollectOutcome::note_if(s.changed, || {
        format!(
            "google books synced — {} volumes on shelves, {} shelf changes, {} annotations across {} accounts",
            s.volumes, s.shelf_events, s.annotations, s.accounts
        )
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_google_books_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-books",
        name: "Google Play Books",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Your bookshelves (Have Read / Reading Now / To Read / Favorites) and Play Books highlights and notes — the cloud sibling of Apple Books.",
        domain: "media",
        vault_path: "books/google/",
        toggleable: true,
        setup: &[],
        caveats: "Reading history is complete and in scope; this covers Google Play Books only, separate from the Apple Books source above.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(GOOGLE_BOOKS_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google"),
    pull: Some(pull),
};

/// [`crate::registry::IntegrationDef::pull`] adapter:
/// [`Vault::google_books_pull`] mapped into the generic outcome shape.
/// Per-account failures never abort the pass — they land in
/// `.trove/google-books-sync.json` — so the headline re-reads the state to
/// surface them rather than reporting a clean sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.google_books_pull()?;
    let errors = vault
        .read_google_books_sync()
        .map(|st| st.accounts.values().filter(|a| a.error.is_some()).count() as u64)
        .unwrap_or(0);
    let mut headline = if s.changed {
        format!(
            "{} volumes on shelves, {} shelf changes, {} annotations",
            s.volumes, s.shelf_events, s.annotations
        )
    } else {
        "library up to date".to_string()
    };
    if errors > 0 {
        headline.push_str(&format!(
            " — {errors} account{} failed",
            if errors == 1 { "" } else { "s" }
        ));
    }
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("accounts", s.accounts as u64),
            ("volumes", s.volumes),
            ("shelf_events", s.shelf_events),
            ("annotations", s.annotations),
            ("account_errors", errors),
        ]),
    })
}

const SYNC_FILE: &str = ".trove/google-books-sync.json";
const LIBRARY_FILE: &str = "books/google/library.jsonl";
const EVENTS_DIR: &str = "books/google/events";
const ANNOTATIONS_DIR: &str = "books/google/annotations";
const INDEX_FILE: &str = "books/google/index.md";
const BOOKS_API: &str = "https://www.googleapis.com";
/// Kept short so a hung connection can't stall the watcher owner loop for
/// long (the `oura.rs` reasoning).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Shelf-volume page size — 40 is the Books API maximum.
const VOLUME_PAGE_SIZE: u32 = 40;
/// Annotations page size — same API maximum.
const ANNOTATION_PAGE_SIZE: u32 = 40;

/// One row of `books/google/library.jsonl`: a volume's membership on one
/// shelf of one account. Only the three key fields are required; everything
/// else defaults so old lines keep deserializing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShelfVolume {
    /// The connected account's email address (data rows key by email, the
    /// mbox `account` pattern; sync state keys by `sub`).
    pub account: String,
    /// Numeric Books API shelf id (built-ins: Favorites=0, To Read=2,
    /// Reading Now=3, Have Read=4, Reviewed=5; custom shelves get high ids).
    pub shelf_id: i64,
    /// Shelf display title as the API lists it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shelf_title: String,
    /// Books API volume id — stable across shelves and accounts.
    pub volume_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    /// `volumeInfo.publishedDate` — a year, year-month, or full date.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn_10: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn_13: Option<String>,
    /// Closest available added-to-shelf time: `userInfo.updated` (when the
    /// user's relationship to the volume last changed), RFC3339 local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,
    /// Everything else the API sent (full fidelity at write time): the
    /// unconsumed `volumeInfo` fields, plus leftover `userInfo` under a
    /// `userInfo` key.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

/// One line of `books/google/annotations/YYYY-MM.jsonl`: a Play Books
/// highlight or note. Deduped by `id` across passes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoogleBookAnnotation {
    /// Partition timestamp: the annotation's own `updated` (else `created`)
    /// converted to RFC3339 local — its true time, not when the pass ran.
    pub ts: String,
    /// The connected account's email address.
    pub account: String,
    /// Books API annotation id — the dedupe key.
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub volume_id: String,
    /// Book title, denormalized from the shelf snapshot when the volume is
    /// shelved (empty for annotations on unshelved books).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Books API `layerId` (e.g. highlight vs. note layers).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layer: String,
    /// The highlighted passage (`selectedText`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// The user's own note text (`data`), when they wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// RFC3339 local creation/update times from the annotation itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

/// One line of `books/google/events/YYYY-MM.jsonl`: a shelf-membership
/// change. A move between shelves is one `removed` plus one `added`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShelfEvent {
    /// RFC3339 local time of the pass that observed the change.
    pub ts: String,
    /// The connected account's email address.
    pub account: String,
    /// "added" or "removed".
    pub kind: String,
    pub shelf_id: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shelf_title: String,
    pub volume_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

/// Per-account sync progress, persisted in `.trove/google-books-sync.json`
/// (keyed by Google `sub`). Deleting an account's entry re-pulls its
/// annotations from the beginning — the id dedupe makes that safe.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleBooksAccountState {
    /// Display address (for the index / UI; the map key is the `sub`).
    #[serde(default)]
    pub email: String,
    /// Annotations high-water mark: the max raw `updated` stamp seen, passed
    /// back as `updatedMin` next pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations_updated_min: Option<String>,
    /// Current volume count per shelf title (drives index.md).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shelf_counts: BTreeMap<String, u64>,
    /// Total (account, shelf, volume) rows in the current snapshot.
    #[serde(default)]
    pub volumes: u64,
    /// Total annotations ever written for this account.
    #[serde(default)]
    pub annotations: u64,
    /// Soft failures from the last pass (a 404ing shelf, an unavailable
    /// annotations API) — the pass still completed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub soft_errors: Vec<String>,
    /// Why this account's last pass failed outright, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The whole Play Books sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleBooksSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Per-account progress, keyed by Google `sub`.
    pub accounts: BTreeMap<String, GoogleBooksAccountState>,
}

/// Result of one Play Books sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GoogleBooksSyncStats {
    /// Accounts whose data changed this pass.
    pub accounts: u32,
    /// Total (account, shelf, volume) rows in the snapshot after the pass.
    pub volumes: u64,
    /// Shelf-membership change events appended this pass.
    pub shelf_events: u64,
    /// Annotations newly written this pass.
    pub annotations: u64,
    /// Anything on disk changed (snapshot rewritten, events or annotations
    /// appended) — drives the watcher-log notice.
    pub changed: bool,
}

/// Status-level fetch errors needing distinct handling.
enum FetchError {
    RateLimited,
    Unauthorized,
    /// Empty/restricted shelves 404 — soft, per shelf.
    NotFound,
    /// Restricted shelves and the older annotations surface 403 — soft.
    Forbidden,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::Forbidden => write!(f, "forbidden (HTTP 403)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// A vault write error inside the API-fetch path → a soft `FetchError`.
fn soft(e: anyhow::Error) -> FetchError {
    FetchError::Other(format!("{e:#}"))
}

/// One shelf as the my-library listing returns it.
struct Shelf {
    id: i64,
    title: String,
}

/// One page of annotations.
struct AnnotationsPage {
    items: Vec<Value>,
    next_token: Option<String>,
}

/// Thin Books API client. The base URL is injected so the orchestration is
/// testable against a local stub (the `oura.rs`/`gmail.rs` pattern).
struct BooksClient {
    base: String,
    token: String,
}

impl BooksClient {
    fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, FetchError> {
        let mut req = ureq::get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HTTP_TIMEOUT);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }

    /// The authenticated my-library shelves: built-ins plus custom shelves.
    fn shelves(&self) -> Result<Vec<Shelf>, FetchError> {
        let v = self.get("/books/v1/mylibrary/bookshelves", &[])?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| {
                        Some(Shelf {
                            id: s.get("id")?.as_i64()?,
                            title: s
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Every volume on one shelf, paged via `startIndex`/`maxResults`. Stops
    /// on the first empty page as well as at `totalItems`, because the
    /// latter is known to overcount.
    fn shelf_volumes(&self, shelf_id: i64) -> Result<Vec<Value>, FetchError> {
        let mut out: Vec<Value> = Vec::new();
        let mut start: u64 = 0;
        loop {
            let v = self.get(
                &format!("/books/v1/mylibrary/bookshelves/{shelf_id}/volumes"),
                &[
                    ("startIndex", start.to_string()),
                    ("maxResults", VOLUME_PAGE_SIZE.to_string()),
                ],
            )?;
            let items = v
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let total = v.get("totalItems").and_then(Value::as_u64).unwrap_or(0);
            let n = items.len() as u64;
            out.extend(items);
            start += n;
            if n == 0 || start >= total {
                break;
            }
        }
        Ok(out)
    }

    /// One page of my-library annotations, optionally incremental from
    /// `updatedMin`.
    fn annotations_page(
        &self,
        updated_min: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<AnnotationsPage, FetchError> {
        let mut params = vec![("maxResults", ANNOTATION_PAGE_SIZE.to_string())];
        if let Some(u) = updated_min {
            params.push(("updatedMin", u.to_string()));
        }
        if let Some(t) = page_token {
            params.push(("pageToken", t.to_string()));
        }
        let v = self.get("/books/v1/mylibrary/annotations", &params)?;
        Ok(AnnotationsPage {
            items: v
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            next_token: v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// An RFC3339 timestamp (the API speaks UTC `Z`) → RFC3339 local, per the
/// vault timestamp convention; unparseable input passes through verbatim.
fn to_local_rfc3339(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| ts.to_string())
}

/// `m.remove(key)` as a plain string (empty when missing or non-string).
fn take_string(m: &mut serde_json::Map<String, Value>, key: &str) -> String {
    match m.remove(key) {
        Some(Value::String(s)) => s,
        _ => String::new(),
    }
}

/// One Books API volume JSON (as a shelf-volumes item) → a snapshot row.
/// `None` when the volume has no id. Normalized fields are *moved out* of
/// `volumeInfo`/`userInfo`; whatever remains lands in `extra` so nothing
/// the source gave us is dropped.
fn shelf_volume_record(
    account: &str,
    shelf_id: i64,
    shelf_title: &str,
    v: &Value,
) -> Option<ShelfVolume> {
    let volume_id = v.get("id")?.as_str()?.to_string();
    let mut info = v
        .get("volumeInfo")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let title = take_string(&mut info, "title");
    let authors = match info.remove("authors") {
        Some(Value::Array(a)) => a
            .into_iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let publisher = take_string(&mut info, "publisher");
    let published = take_string(&mut info, "publishedDate");
    let page_count = info.remove("pageCount").and_then(|x| x.as_u64());
    let (mut isbn_10, mut isbn_13) = (None, None);
    if let Some(Value::Array(ids)) = info.remove("industryIdentifiers") {
        for id in &ids {
            let val = id.get("identifier").and_then(Value::as_str);
            match id.get("type").and_then(Value::as_str) {
                Some("ISBN_10") => isbn_10 = val.map(str::to_string),
                Some("ISBN_13") => isbn_13 = val.map(str::to_string),
                _ => {}
            }
        }
    }
    let mut user = v
        .get("userInfo")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let added = match take_string(&mut user, "updated") {
        s if s.is_empty() => None,
        s => Some(to_local_rfc3339(&s)),
    };
    let mut extra = info;
    if !user.is_empty() {
        extra.insert("userInfo".to_string(), Value::Object(user));
    }
    Some(ShelfVolume {
        account: account.to_string(),
        shelf_id,
        shelf_title: shelf_title.to_string(),
        volume_id,
        title,
        authors,
        publisher,
        published,
        page_count,
        isbn_10,
        isbn_13,
        added,
        extra,
    })
}

/// One Books API annotation JSON → a stream record. `None` when it has no
/// id. The record's `ts` is the annotation's own updated/created time (its
/// true time), falling back to the pass time only when the API sent neither.
fn annotation_record(
    account: &str,
    v: &Value,
    titles: &BTreeMap<String, String>,
    fallback_ts: &str,
) -> Option<GoogleBookAnnotation> {
    let id = v.get("id")?.as_str()?.to_string();
    let volume_id = v
        .get("volumeId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let created = v
        .get("created")
        .and_then(Value::as_str)
        .map(to_local_rfc3339);
    let updated = v
        .get("updated")
        .and_then(Value::as_str)
        .map(to_local_rfc3339);
    let ts = updated
        .clone()
        .or_else(|| created.clone())
        .unwrap_or_else(|| fallback_ts.to_string());
    Some(GoogleBookAnnotation {
        ts,
        account: account.to_string(),
        title: titles.get(&volume_id).cloned().unwrap_or_default(),
        volume_id,
        id,
        layer: v
            .get("layerId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        text: v
            .get("selectedText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        note: v
            .get("data")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        created,
        updated,
    })
}

/// The annotations high-water mark after observing `observed` (a raw API
/// `updated` stamp): the lexical max — all stamps are UTC `Z`, so lexical
/// order is chronological. Never moves backwards.
fn advance_hwm(current: Option<String>, observed: Option<&str>) -> Option<String> {
    match (current, observed) {
        (cur, None) => cur,
        (None, Some(o)) => Some(o.to_string()),
        (Some(c), Some(o)) => Some(if o > c.as_str() { o.to_string() } else { c }),
    }
}

/// One shelf's pull outcome: `volumes: None` means the shelf fetch soft-
/// failed and its old rows must be carried forward (never turned into
/// `removed` events).
struct ShelfPull {
    shelf_id: i64,
    volumes: Option<Vec<ShelfVolume>>,
}

/// The pure membership merge for one account: old snapshot rows + this
/// pass's shelf pulls → (new rows sorted by (shelf, volume), change events).
///
/// - `old` empty = this account's **baseline**: rows land, no events.
/// - A shelf with `volumes: None` (soft failure) carries its old rows
///   forward and is excluded from the diff entirely.
/// - A shelf present in `old` but absent from `pulls` (a deleted custom
///   shelf) emits `removed` for every row it held.
fn merge_account_shelves(
    old: &[ShelfVolume],
    pulls: &[ShelfPull],
    ts: &str,
) -> (Vec<ShelfVolume>, Vec<ShelfEvent>) {
    let failed: HashSet<i64> = pulls
        .iter()
        .filter(|p| p.volumes.is_none())
        .map(|p| p.shelf_id)
        .collect();
    let mut rows: Vec<ShelfVolume> = Vec::new();
    for p in pulls {
        match &p.volumes {
            Some(vols) => rows.extend(vols.iter().cloned()),
            None => rows.extend(old.iter().filter(|r| r.shelf_id == p.shelf_id).cloned()),
        }
    }
    rows.sort_by(|a, b| {
        (a.shelf_id, a.volume_id.as_str()).cmp(&(b.shelf_id, b.volume_id.as_str()))
    });

    let mut events = Vec::new();
    if !old.is_empty() {
        let event = |r: &ShelfVolume, kind: &str| ShelfEvent {
            ts: ts.to_string(),
            account: r.account.clone(),
            kind: kind.to_string(),
            shelf_id: r.shelf_id,
            shelf_title: r.shelf_title.clone(),
            volume_id: r.volume_id.clone(),
            title: r.title.clone(),
        };
        let old_set: HashSet<(i64, &str)> =
            old.iter().map(|r| (r.shelf_id, r.volume_id.as_str())).collect();
        let new_set: HashSet<(i64, &str)> =
            rows.iter().map(|r| (r.shelf_id, r.volume_id.as_str())).collect();
        for r in &rows {
            if !failed.contains(&r.shelf_id) && !old_set.contains(&(r.shelf_id, r.volume_id.as_str())) {
                events.push(event(r, "added"));
            }
        }
        for r in old {
            if !failed.contains(&r.shelf_id) && !new_set.contains(&(r.shelf_id, r.volume_id.as_str())) {
                events.push(event(r, "removed"));
            }
        }
    }
    (rows, events)
}

/// Account-pass tallies fed back into the pass-wide stats.
struct AccountPass {
    shelf_events: u64,
    annotations: u64,
    changed: bool,
}

/// Map a status-level Books API error to a user-facing message for the
/// sync state.
fn books_error(account: &str, e: FetchError) -> String {
    match e {
        FetchError::RateLimited => {
            format!("Books API rate limited the sync ({account}) — it resumes next pass")
        }
        FetchError::Unauthorized => {
            format!("Books API rejected the token ({account}, 401) — reconnect from the Integrations tab")
        }
        other => format!("google books {account}: {other}"),
    }
}

impl Vault {
    /// One Play Books sync pass across every connected Google account:
    /// refresh each account's token, pull its shelves and their volumes,
    /// diff membership against the snapshot (appending change events),
    /// rewrite the snapshot, then pull annotations incrementally from the
    /// `updatedMin` high-water mark. A silent no-op when no Google account
    /// is connected. Per-account failures are recorded in
    /// `.trove/google-books-sync.json` and never abort other accounts.
    pub fn collect_google_books(
        &self,
        now: chrono::DateTime<Local>,
    ) -> Result<GoogleBooksSyncStats> {
        let accounts = self.google_status()?.accounts;
        let mut state = self.read_google_books_sync().unwrap_or_default();
        // Forget state for accounts that have been disconnected. Their
        // snapshot rows stay — vault data stays complete.
        let live: HashSet<&str> = accounts.iter().map(|a| a.sub.as_str()).collect();
        state.accounts.retain(|sub, _| live.contains(sub.as_str()));

        if accounts.is_empty() {
            // Persist the pruning above so a disconnected account's row
            // doesn't linger in the state or the index.
            if self.resolve(SYNC_FILE).map(|p| p.exists()).unwrap_or(false) {
                state.updated = now.to_rfc3339();
                self.write_google_books_sync(&state)?;
                self.write_google_books_index(&state)?;
            }
            return Ok(GoogleBooksSyncStats::default());
        }

        let now_ts = now.to_rfc3339();
        let mut library: Vec<ShelfVolume> = self.read_snapshot(LIBRARY_FILE)?;
        let before = library.clone();
        // volume id → title, for denormalizing titles onto annotations.
        let mut titles: BTreeMap<String, String> = library
            .iter()
            .filter(|r| !r.title.is_empty())
            .map(|r| (r.volume_id.clone(), r.title.clone()))
            .collect();
        // The annotation dedupe set, rebuilt by scanning the stream — a lost
        // cursor can never duplicate rows.
        let mut seen = self.google_books_annotation_ids()?;
        let mut stats = GoogleBooksSyncStats::default();

        for acct in &accounts {
            // A flagged account can't refresh non-interactively; skip it
            // (the card surfaces the reconnect prompt).
            if acct.needs_reconnect {
                continue;
            }
            let token = match crate::sync::google::fresh_token(self, &acct.sub) {
                Ok(t) => t.access_token,
                Err(e) => {
                    let astate = state.accounts.entry(acct.sub.clone()).or_default();
                    astate.email = acct.email.clone();
                    astate.error = Some(format!("{e:#}"));
                    continue;
                }
            };
            let client = BooksClient { base: BOOKS_API.to_string(), token };
            {
                let astate = state.accounts.entry(acct.sub.clone()).or_default();
                astate.email = acct.email.clone();
                astate.error = None;
                astate.soft_errors.clear();
            }
            match self.google_books_sync_account(
                &client, &acct.sub, &acct.email, &mut state, &mut library, &mut titles,
                &mut seen, &now_ts,
            ) {
                Ok(pass) => {
                    if pass.changed {
                        stats.accounts += 1;
                    }
                    stats.shelf_events += pass.shelf_events;
                    stats.annotations += pass.annotations;
                }
                Err(e) => {
                    let astate = state.accounts.entry(acct.sub.clone()).or_default();
                    astate.error = Some(books_error(&acct.email, e));
                }
            }
            // Persist after each account so an interrupted pass keeps what
            // it learned (high-water marks, errors).
            self.write_google_books_sync(&state)?;
        }

        library.sort_by(|a, b| {
            (a.account.as_str(), a.shelf_id, a.volume_id.as_str()).cmp(&(
                b.account.as_str(),
                b.shelf_id,
                b.volume_id.as_str(),
            ))
        });
        if library != before {
            self.write_snapshot(LIBRARY_FILE, &library)?;
            stats.changed = true;
        }
        stats.volumes = library.len() as u64;
        stats.changed |= stats.shelf_events + stats.annotations > 0;
        state.updated = now_ts;
        self.write_google_books_sync(&state)?;
        self.write_google_books_index(&state)?;
        Ok(stats)
    }

    /// Pull every connected account's Play Books library into the vault —
    /// the manual "Sync now" path. The very same pass as the scheduled one
    /// ([`Self::collect_google_books`]), so there is exactly one writer of
    /// `books/google/`; only the no-account case differs (a clean error
    /// instead of a silent no-op). Blocking (network).
    pub fn google_books_pull(&self) -> Result<GoogleBooksSyncStats> {
        if self.google_status()?.accounts.is_empty() {
            bail!("no Google account is connected");
        }
        self.collect_google_books(Local::now())
    }

    /// Shelves then annotations for one account. Per-shelf and annotation
    /// failures are soft (recorded in `soft_errors`, old rows carried
    /// forward); only a 401 or a failed shelf *listing* fails the account.
    #[allow(clippy::too_many_arguments)]
    fn google_books_sync_account(
        &self,
        client: &BooksClient,
        sub: &str,
        email: &str,
        state: &mut GoogleBooksSyncState,
        library: &mut Vec<ShelfVolume>,
        titles: &mut BTreeMap<String, String>,
        seen: &mut HashSet<String>,
        now_ts: &str,
    ) -> Result<AccountPass, FetchError> {
        let shelves = client.shelves()?;
        let mut pulls = Vec::with_capacity(shelves.len());
        let mut soft_errors = Vec::new();
        for shelf in &shelves {
            match client.shelf_volumes(shelf.id) {
                Ok(items) => pulls.push(ShelfPull {
                    shelf_id: shelf.id,
                    volumes: Some(
                        items
                            .iter()
                            .filter_map(|v| shelf_volume_record(email, shelf.id, &shelf.title, v))
                            .collect(),
                    ),
                }),
                Err(FetchError::Unauthorized) => return Err(FetchError::Unauthorized),
                Err(e) => {
                    // Empty/restricted shelves 404 or 403 — soft: record,
                    // carry the old rows forward, keep going.
                    soft_errors.push(format!("shelf \"{}\" ({}): {e}", shelf.title, shelf.id));
                    pulls.push(ShelfPull { shelf_id: shelf.id, volumes: None });
                }
            }
        }

        let old: Vec<ShelfVolume> =
            library.iter().filter(|r| r.account == email).cloned().collect();
        let (rows, events) = merge_account_shelves(&old, &pulls, now_ts);
        if !events.is_empty() {
            self.stream(EVENTS_DIR, Partition::Month)
                .append(&events, |e| &e.ts)
                .map_err(soft)?;
        }
        for r in &rows {
            if !r.title.is_empty() {
                titles.insert(r.volume_id.clone(), r.title.clone());
            }
        }
        let rows_changed = rows != old;
        library.retain(|r| r.account != email);
        library.extend(rows.iter().cloned());

        let mut shelf_counts: BTreeMap<String, u64> = BTreeMap::new();
        for r in &rows {
            let key = if r.shelf_title.is_empty() {
                format!("shelf {}", r.shelf_id)
            } else {
                r.shelf_title.clone()
            };
            *shelf_counts.entry(key).or_default() += 1;
        }

        // Annotations — an older API surface; its unavailability (403) must
        // never fail the shelves pull above.
        let hwm = state.accounts.get(sub).and_then(|s| s.annotations_updated_min.clone());
        let mut written = 0u64;
        let mut new_hwm = hwm.clone();
        match self.google_books_pull_annotations(client, email, hwm.as_deref(), titles, seen, now_ts)
        {
            Ok((w, observed)) => {
                written = w;
                new_hwm = advance_hwm(new_hwm, observed.as_deref());
            }
            Err(FetchError::Unauthorized) => return Err(FetchError::Unauthorized),
            Err(e) => soft_errors.push(format!("annotations: {e}")),
        }

        let astate = state.accounts.entry(sub.to_string()).or_default();
        astate.email = email.to_string();
        astate.annotations_updated_min = new_hwm;
        astate.shelf_counts = shelf_counts;
        astate.volumes = rows.len() as u64;
        astate.annotations += written;
        astate.soft_errors = soft_errors;
        Ok(AccountPass {
            shelf_events: events.len() as u64,
            annotations: written,
            changed: rows_changed || written > 0,
        })
    }

    /// Walk every annotations page from `updated_min`, normalize, dedupe by
    /// id, append. Returns `(written, max raw updated stamp observed)`.
    fn google_books_pull_annotations(
        &self,
        client: &BooksClient,
        email: &str,
        updated_min: Option<&str>,
        titles: &BTreeMap<String, String>,
        seen: &mut HashSet<String>,
        now_ts: &str,
    ) -> Result<(u64, Option<String>), FetchError> {
        let mut records = Vec::new();
        let mut hwm: Option<String> = None;
        let mut page_token: Option<String> = None;
        loop {
            let page = client.annotations_page(updated_min, page_token.as_deref())?;
            for item in &page.items {
                hwm = advance_hwm(hwm, item.get("updated").and_then(Value::as_str));
                if let Some(rec) = annotation_record(email, item, titles, now_ts) {
                    records.push(rec);
                }
            }
            match page.next_token {
                Some(t) => page_token = Some(t),
                None => break,
            }
        }
        let written = self.append_google_books_annotations(records, seen).map_err(soft)?;
        Ok((written, hwm))
    }

    /// Append annotations the stream doesn't already hold (deduped by id via
    /// `seen`, which the caller seeds from [`Self::google_books_annotation_ids`]
    /// and which grows as we write). Returns how many were written.
    fn append_google_books_annotations(
        &self,
        mut records: Vec<GoogleBookAnnotation>,
        seen: &mut HashSet<String>,
    ) -> Result<u64> {
        records.retain(|r| seen.insert(r.id.clone()));
        if !records.is_empty() {
            self.stream(ANNOTATIONS_DIR, Partition::Month)
                .append(&records, |r| &r.ts)?;
        }
        Ok(records.len() as u64)
    }

    /// Every annotation id already in the stream — the dedupe set, rebuilt
    /// by scanning so a lost sync-state file can never duplicate rows.
    fn google_books_annotation_ids(&self) -> Result<HashSet<String>> {
        let stream = self.stream(ANNOTATIONS_DIR, Partition::Month);
        let mut out = HashSet::new();
        for p in stream.partitions()? {
            for r in stream.read::<GoogleBookAnnotation>(&p)? {
                out.insert(r.id);
            }
        }
        Ok(out)
    }

    /// The persisted Play Books sync progress, if a sync has ever run.
    pub fn read_google_books_sync(&self) -> Option<GoogleBooksSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Atomic write so readers never see a torn file — called after every
    /// account so an interrupted pass keeps its high-water marks.
    fn write_google_books_sync(&self, state: &GoogleBooksSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Regenerate the human-readable summary at `books/google/index.md`.
    fn write_google_books_index(&self, state: &GoogleBooksSyncState) -> Result<()> {
        let mut md = format!("# Google Play Books\n\nLast sync: {}\n", state.updated);
        for s in state.accounts.values() {
            md.push_str(&format!("\n## {}\n\n", s.email));
            if s.shelf_counts.is_empty() {
                md.push_str("No shelves.\n");
            } else {
                md.push_str("| Shelf | Volumes |\n|---|---|\n");
                for (title, n) in &s.shelf_counts {
                    md.push_str(&format!("| {title} | {n} |\n"));
                }
            }
            md.push_str(&format!("\nAnnotations: {}\n", s.annotations));
            if let Some(e) = &s.error {
                md.push_str(&format!("\nError: {e}\n"));
            }
            for e in &s.soft_errors {
                md.push_str(&format!("\nWarning: {e}\n"));
            }
        }
        crate::store::write_atomic(&self.resolve(INDEX_FILE)?, md.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-gbooks-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    const TS: &str = "2026-06-12T09:00:00-07:00";

    fn vol(shelf_id: i64, shelf_title: &str, volume_id: &str, title: &str) -> ShelfVolume {
        ShelfVolume {
            account: "me@gmail.com".into(),
            shelf_id,
            shelf_title: shelf_title.into(),
            volume_id: volume_id.into(),
            title: title.into(),
            authors: Vec::new(),
            publisher: String::new(),
            published: String::new(),
            page_count: None,
            isbn_10: None,
            isbn_13: None,
            added: None,
            extra: serde_json::Map::new(),
        }
    }

    fn ann(id: &str, ts: &str) -> GoogleBookAnnotation {
        GoogleBookAnnotation {
            ts: ts.into(),
            account: "me@gmail.com".into(),
            id: id.into(),
            volume_id: "VOL1".into(),
            title: "Death & Co".into(),
            layer: "highlights".into(),
            text: "30ml gin".into(),
            note: None,
            created: Some(ts.into()),
            updated: Some(ts.into()),
        }
    }

    #[test]
    fn shelf_volume_record_normalizes_isbns_and_extra() {
        let api = json!({
            "id": "VOL1",
            "volumeInfo": {
                "title": "Death & Co",
                "authors": ["David Kaplan", "Nick Fauchald"],
                "publisher": "Ten Speed Press",
                "publishedDate": "2014-10-07",
                "pageCount": 320,
                "industryIdentifiers": [
                    {"type": "ISBN_10", "identifier": "1607745259"},
                    {"type": "ISBN_13", "identifier": "9781607745259"}
                ],
                "language": "en"
            },
            "userInfo": {"updated": "2026-06-01T12:00:00.000Z", "isPurchased": true}
        });
        let r = shelf_volume_record("me@gmail.com", 4, "Have Read", &api).unwrap();
        assert_eq!(r.volume_id, "VOL1");
        assert_eq!((r.shelf_id, r.shelf_title.as_str()), (4, "Have Read"));
        assert_eq!(r.title, "Death & Co");
        assert_eq!(r.authors, vec!["David Kaplan", "Nick Fauchald"]);
        assert_eq!(r.publisher, "Ten Speed Press");
        assert_eq!(r.published, "2014-10-07");
        assert_eq!(r.page_count, Some(320));
        assert_eq!(r.isbn_10.as_deref(), Some("1607745259"));
        assert_eq!(r.isbn_13.as_deref(), Some("9781607745259"));
        // userInfo.updated → added, converted to local but same instant.
        let added = chrono::DateTime::parse_from_rfc3339(r.added.as_deref().unwrap()).unwrap();
        assert_eq!(
            added.timestamp(),
            chrono::DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z").unwrap().timestamp()
        );
        // Unconsumed fields survive in extra — nothing dropped.
        assert_eq!(r.extra.get("language"), Some(&json!("en")));
        assert_eq!(r.extra["userInfo"]["isPurchased"], json!(true));
        assert!(r.extra["userInfo"].get("updated").is_none(), "consumed into added");

        // Sparse volume: no ISBNs, no optionals → the keys are omitted.
        let sparse = json!({"id": "VOL2", "volumeInfo": {"title": "Slim"}});
        let r = shelf_volume_record("me@gmail.com", 2, "To Read", &sparse).unwrap();
        let line = serde_json::to_string(&r).unwrap();
        for absent in ["isbn_10", "isbn_13", "page_count", "publisher", "added", "extra", "authors"] {
            assert!(!line.contains(absent), "{absent} should be omitted: {line}");
        }

        // No id → no record.
        assert!(shelf_volume_record("me@gmail.com", 0, "Favorites", &json!({})).is_none());
    }

    #[test]
    fn annotation_record_highlight_note_and_title_join() {
        let titles: BTreeMap<String, String> =
            [("VOL1".to_string(), "Death & Co".to_string())].into();
        let api = json!({
            "id": "ANN1",
            "volumeId": "VOL1",
            "layerId": "highlights",
            "selectedText": "stir, never shake",
            "data": "try this at home",
            "created": "2026-05-30T08:00:00.000Z",
            "updated": "2026-06-01T09:30:00.000Z"
        });
        let r = annotation_record("me@gmail.com", &api, &titles, TS).unwrap();
        assert_eq!(r.id, "ANN1");
        assert_eq!(r.title, "Death & Co", "title denormalized from the shelf snapshot");
        assert_eq!(r.layer, "highlights");
        assert_eq!(r.text, "stir, never shake");
        assert_eq!(r.note.as_deref(), Some("try this at home"));
        // ts is the annotation's own updated time (localized), and it must
        // carry a partitionable month prefix.
        let ts = chrono::DateTime::parse_from_rfc3339(&r.ts).unwrap();
        assert_eq!(
            ts.timestamp(),
            chrono::DateTime::parse_from_rfc3339("2026-06-01T09:30:00Z").unwrap().timestamp()
        );
        assert!(Partition::Month.key(&r.ts).is_some());

        // Highlight without a note: `note` omitted, ts falls back to
        // created when updated is missing, then to the pass time.
        let bare = json!({"id": "ANN2", "volumeId": "VOLX", "selectedText": "gin"});
        let r = annotation_record("me@gmail.com", &bare, &titles, TS).unwrap();
        assert!(r.note.is_none());
        assert!(r.title.is_empty(), "unshelved volume has no title to join");
        assert_eq!(r.ts, TS, "no created/updated → pass time");
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("note") && !line.contains("\"title\""), "{line}");

        assert!(annotation_record("me@gmail.com", &json!({}), &titles, TS).is_none());
    }

    #[test]
    fn shelf_diff_emits_adds_removes_and_moves() {
        // Baseline: no old rows → rows land, no events.
        let pulls = vec![ShelfPull { shelf_id: 2, volumes: Some(vec![vol(2, "To Read", "V1", "Book One")]) }];
        let (rows, events) = merge_account_shelves(&[], &pulls, TS);
        assert_eq!(rows.len(), 1);
        assert!(events.is_empty(), "first sight of an account is a silent baseline");

        // V1 moves To Read → Have Read; V2 newly added; V3 dropped entirely.
        let old = vec![
            vol(2, "To Read", "V1", "Book One"),
            vol(2, "To Read", "V3", "Book Three"),
        ];
        let pulls = vec![
            ShelfPull { shelf_id: 2, volumes: Some(vec![vol(2, "To Read", "V2", "Book Two")]) },
            ShelfPull { shelf_id: 4, volumes: Some(vec![vol(4, "Have Read", "V1", "Book One")]) },
        ];
        let (rows, events) = merge_account_shelves(&old, &pulls, TS);
        assert_eq!(rows.len(), 2);
        let mut kinds: Vec<(String, i64, String)> = events
            .iter()
            .map(|e| (e.kind.clone(), e.shelf_id, e.volume_id.clone()))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ("added".to_string(), 2, "V2".to_string()),
                ("added".to_string(), 4, "V1".to_string()),
                ("removed".to_string(), 2, "V1".to_string()),
                ("removed".to_string(), 2, "V3".to_string()),
            ],
            "a move is one removed + one added; every event carries its shelf"
        );
        assert!(events.iter().all(|e| e.ts == TS && e.account == "me@gmail.com"));

        // A shelf in old but absent from pulls (deleted custom shelf).
        let old = vec![vol(99, "Custom", "V9", "Niner")];
        let (rows, events) = merge_account_shelves(&old, &[], TS);
        assert!(rows.is_empty());
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].kind.as_str(), events[0].volume_id.as_str()), ("removed", "V9"));
    }

    #[test]
    fn failed_shelf_is_soft_carries_forward_and_emits_nothing() {
        let old = vec![
            vol(2, "To Read", "V1", "Book One"),
            vol(4, "Have Read", "V2", "Book Two"),
        ];
        // Shelf 2's fetch failed (volumes: None); shelf 4 succeeded and
        // gained V3.
        let pulls = vec![
            ShelfPull { shelf_id: 2, volumes: None },
            ShelfPull {
                shelf_id: 4,
                volumes: Some(vec![
                    vol(4, "Have Read", "V2", "Book Two"),
                    vol(4, "Have Read", "V3", "Book Three"),
                ]),
            },
        ];
        let (rows, events) = merge_account_shelves(&old, &pulls, TS);
        assert_eq!(rows.len(), 3, "failed shelf's old row carried forward");
        assert!(rows.iter().any(|r| r.shelf_id == 2 && r.volume_id == "V1"));
        assert_eq!(events.len(), 1, "no removed events fabricated for the failed shelf");
        assert_eq!((events[0].kind.as_str(), events[0].volume_id.as_str()), ("added", "V3"));
    }

    #[test]
    fn annotation_dedupe_across_passes_and_hwm_advance() {
        let v = temp_vault("dedupe");
        let mut seen = v.google_books_annotation_ids().unwrap();
        assert!(seen.is_empty());
        let written = v
            .append_google_books_annotations(
                vec![ann("A", "2026-05-30T08:00:00-07:00"), ann("B", TS)],
                &mut seen,
            )
            .unwrap();
        assert_eq!(written, 2);

        // Next pass: the seen set is rebuilt by scanning the stream (a lost
        // cursor never duplicates), and the overlap is dropped.
        let mut seen = v.google_books_annotation_ids().unwrap();
        assert_eq!(seen.len(), 2);
        let written = v
            .append_google_books_annotations(vec![ann("B", TS), ann("C", TS)], &mut seen)
            .unwrap();
        assert_eq!(written, 1, "only the unseen annotation lands");

        // Partitioned by the annotation's own month; B appears exactly once.
        let stream = v.stream(ANNOTATIONS_DIR, Partition::Month);
        assert_eq!(stream.partitions().unwrap(), vec!["2026-05", "2026-06"]);
        let june: Vec<GoogleBookAnnotation> = stream.read("2026-06").unwrap();
        assert_eq!(
            june.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["B", "C"]
        );

        // High-water mark: lexical max of raw UTC stamps, never backwards.
        assert_eq!(advance_hwm(None, None), None);
        assert_eq!(
            advance_hwm(None, Some("2026-06-01T00:00:00.000Z")).as_deref(),
            Some("2026-06-01T00:00:00.000Z")
        );
        assert_eq!(
            advance_hwm(
                Some("2026-06-01T00:00:00.000Z".into()),
                Some("2026-06-02T00:00:00.000Z")
            )
            .as_deref(),
            Some("2026-06-02T00:00:00.000Z")
        );
        assert_eq!(
            advance_hwm(
                Some("2026-06-02T00:00:00.000Z".into()),
                Some("2026-06-01T00:00:00.000Z")
            )
            .as_deref(),
            Some("2026-06-02T00:00:00.000Z"),
            "stale observation never moves the mark backwards"
        );
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        assert!(v.read_google_books_sync().is_none());
        let mut state = GoogleBooksSyncState {
            updated: "2026-06-12T10:00:00-07:00".into(),
            ..Default::default()
        };
        state.accounts.insert(
            "12345".into(),
            GoogleBooksAccountState {
                email: "me@gmail.com".into(),
                annotations_updated_min: Some("2026-06-01T09:30:00.000Z".into()),
                shelf_counts: [("Have Read".to_string(), 12u64)].into(),
                volumes: 12,
                annotations: 42,
                soft_errors: vec!["shelf \"Reviewed\" (5): not found (HTTP 404)".into()],
                error: None,
            },
        );
        v.write_google_books_sync(&state).unwrap();
        let loaded = v.read_google_books_sync().unwrap();
        assert_eq!(loaded.updated, "2026-06-12T10:00:00-07:00");
        let a = &loaded.accounts["12345"];
        assert_eq!(a.email, "me@gmail.com");
        assert_eq!(a.annotations_updated_min.as_deref(), Some("2026-06-01T09:30:00.000Z"));
        assert_eq!(a.shelf_counts["Have Read"], 12);
        assert_eq!((a.volumes, a.annotations), (12, 42));
        assert_eq!(a.soft_errors.len(), 1);
        assert!(!v.root().join(".trove/google-books-sync.json.tmp").exists());
    }

    #[test]
    fn collect_without_an_account_is_a_silent_noop() {
        let v = temp_vault("noaccount");
        let stats = v.collect_google_books(Local::now()).unwrap();
        assert_eq!(stats.volumes + stats.shelf_events + stats.annotations, 0);
        assert!(!stats.changed);
        assert!(v.read_google_books_sync().is_none(), "no state file materializes");
        assert!(!v.root().join("books/google").exists(), "no store directory either");
    }

    #[test]
    fn pull_without_an_account_is_a_clean_error() {
        // Unlike the silent scheduled pass, a user-triggered pull must say
        // why nothing happened.
        let v = temp_vault("pull-noaccount");
        let err = pull(&v).unwrap_err();
        assert!(err.to_string().contains("no Google account"), "{err}");
    }

    #[test]
    fn index_lists_shelves_annotations_and_errors() {
        let v = temp_vault("index");
        let mut state = GoogleBooksSyncState {
            updated: "2026-06-12T10:00:00-07:00".into(),
            ..Default::default()
        };
        state.accounts.insert(
            "1".into(),
            GoogleBooksAccountState {
                email: "me@gmail.com".into(),
                shelf_counts: [
                    ("Have Read".to_string(), 12u64),
                    ("Reading Now".to_string(), 2u64),
                ]
                .into(),
                volumes: 14,
                annotations: 42,
                soft_errors: vec!["annotations: forbidden (HTTP 403)".into()],
                ..Default::default()
            },
        );
        v.write_google_books_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join("books/google/index.md")).unwrap();
        assert!(md.contains("# Google Play Books"));
        assert!(md.contains("## me@gmail.com"));
        assert!(md.contains("| Have Read | 12 |"));
        assert!(md.contains("| Reading Now | 2 |"));
        assert!(md.contains("Annotations: 42"));
        assert!(md.contains("Warning: annotations: forbidden (HTTP 403)"));
    }
}
