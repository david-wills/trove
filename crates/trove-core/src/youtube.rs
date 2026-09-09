//! YouTube collector — subscriptions, playlists (including Liked videos),
//! and own uploads from every connected Google account, into the new
//! `youtube/` store. The OAuth side (connect, token store, refresh,
//! reconnect flagging) is owned by [`crate::sync::google`]; this module only
//! asks it for a fresh token per account via
//! [`crate::sync::google::fresh_token`]. See [`crate::gmail`] for the worked
//! per-account-pull template.
//!
//! **Target layout** — current state as snapshots, history as events:
//!
//! - `youtube/subscriptions.jsonl` — snapshot, one row per (account,
//!   channel), atomically rewritten each pass, sorted by (account,
//!   channel id) so rewrites diff cleanly.
//! - `youtube/playlists.jsonl` — snapshot, one row per (account, playlist):
//!   id, title, kind (`liked` / `uploads` / `user`), item count.
//! - `youtube/playlist-items/<playlist-id>.jsonl` — per-playlist membership
//!   snapshot, one row per video, sorted by video id.
//! - `youtube/events/YYYY-MM.jsonl` — append-only history stream of
//!   membership diffs: `subscribed` / `unsubscribed`, `playlist-added` /
//!   `playlist-removed`, `item-added` / `item-removed`.
//!
//! **Why snapshots *plus* an event stream** (the [`crate::tasks`]
//! diff-to-events idea): Liked videos especially are accretive personal
//! history — a video liked and later unliked must not silently vanish from
//! the vault. Each pass diffs the fresh membership against the previous
//! snapshot and appends what changed to `events/` before rewriting the
//! snapshot, so the snapshot stays a clean "current library" view while the
//! stream keeps everything that ever passed through it. The diff baseline
//! *is* the snapshot files, so a lost `.trove` state file can never
//! duplicate history (the conventions' rebuildable-cursor rule).
//!
//! Event timestamps are the source's when it knows one — a playlist item's
//! `publishedAt` is the moment it was added (for Liked videos: the moment of
//! the like), and a subscription's `publishedAt` is the subscribe time — so
//! the *first* sync retroactively materializes a like/subscribe timeline
//! into the right historical months. Removal times aren't observable; those
//! events carry the sync time (bounded by the hourly cadence). A playlist
//! that 404s mid-pass (hidden, or deleted between listing and walking) is
//! skipped without diffing, so a temporarily unreadable playlist can't fake
//! a mass removal; a *deleted* playlist gets a `playlist-removed` event and
//! its `playlist-items/` snapshot is kept frozen — raw vault data stays
//! complete.
//!
//! **The pass, per account** (resumable, request-budgeted like
//! [`crate::gmail`]): page `subscriptions.list?mine=true`; one
//! `channels.list?mine=true` for the `relatedPlaylists` (likes + uploads)
//! plus one `playlists.list?id=…` for their metadata; page
//! `playlists.list?mine=true`; then walk each playlist's
//! `playlistItems.list` pages. Every page costs one budget unit (each of
//! these list calls is 1 YouTube quota unit against the 10k/day default);
//! cursors *and* staged rows persist to `.trove/youtube-sync.json` after
//! every page, so a budget-capped pass resumes exactly where it stopped
//! without re-fetching finished phases. Membership is only ever diffed from
//! a *complete* walk — a partial fetch can't masquerade as removals.
//! Watch history is not in the API (dead since 2016) — see the card caveat.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

/// Seconds between YouTube passes in the watcher loop. Subscriptions and
/// likes churn slowly; hourly is plenty.
pub const YOUTUBE_SYNC_SECS: u64 = 3600;

/// Request budget for one watcher-loop pass. Every list call is 1 YouTube
/// quota unit, so 200 calls/hour is well inside the 10k/day default while
/// covering ~10k playlist items per pass; a giant Liked-videos backfill just
/// takes a few passes (cursors + staged rows persist between them).
pub const YOUTUBE_LOOP_BUDGET: u32 = 200;

// One cheap re-list when nothing changed; request-budgeted so a huge
// first-pass playlist walk can't monopolize the owner loop. A silent no-op
// when no Google account is connected.
fn def_collect(vault: &Vault, _now: chrono::DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_youtube(Some(YOUTUBE_LOOP_BUDGET))?;
    Ok(crate::registry::CollectOutcome::note_if(s.events > 0, || {
        format!(
            "youtube synced — {} library changes across {} accounts",
            s.events, s.accounts
        )
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_youtube_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-youtube",
        name: "YouTube",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Your subscriptions, playlists (including Liked videos), and own uploads from each connected account.",
        domain: "media",
        vault_path: "youtube/",
        toggleable: true,
        setup: &[],
        caveats: "Watch history is not available from the YouTube API (removed in 2016) — only Takeout or the Data Portability API can reach it, neither of which is wired up yet.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(YOUTUBE_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google"),
    pull: Some(pull),
};

/// [`crate::registry::IntegrationDef::pull`] adapter: the unbudgeted manual
/// pull ([`Vault::youtube_pull`]), mapped into the generic outcome shape.
/// Per-account failures never abort the pass — they land in
/// `.trove/youtube-sync.json` — so the headline re-reads the state to
/// surface them rather than reporting a clean sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.youtube_pull()?;
    let errors = vault
        .read_youtube_sync()
        .map(|st| st.accounts.values().filter(|a| a.error.is_some()).count() as u64)
        .unwrap_or(0);
    let mut headline = if s.events > 0 {
        format!("{} library changes across {} accounts", s.events, s.accounts)
    } else {
        "library up to date".to_string()
    };
    if !s.complete {
        // The manual pull is unbudgeted, so an unfinished phase walk means
        // an account failed mid-pass; the cursors resume next pass.
        headline.push_str(" — sync incomplete, resumes next pass");
    }
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
            ("events", s.events),
            ("account_errors", errors),
        ]),
    })
}

const SYNC_FILE: &str = ".trove/youtube-sync.json";
const SUBS_FILE: &str = "youtube/subscriptions.jsonl";
const PLAYLISTS_FILE: &str = "youtube/playlists.jsonl";
const ITEMS_DIR: &str = "youtube/playlist-items";
const EVENTS_DIR: &str = "youtube/events";
const INDEX_FILE: &str = "youtube/index.md";
const YOUTUBE_API: &str = "https://youtube.googleapis.com";
/// Kept short so a hung connection can't stall the watcher owner loop for
/// long (the `oura.rs` reasoning).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Page size for every list endpoint (50 is the API maximum).
const PAGE_SIZE: u32 = 50;

/// One subscription, as stored in `youtube/subscriptions.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SubscriptionRow {
    /// Connected account's address — N accounts coexist in one snapshot.
    pub account: String,
    pub channel_id: String,
    pub title: String,
    /// When the subscription was made (the API's `publishedAt`), RFC3339.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subscribed_at: String,
    /// Everything else the API gave us (description, item counts, …) —
    /// full fidelity at write time.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One playlist, as stored in `youtube/playlists.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PlaylistRow {
    pub account: String,
    pub id: String,
    pub title: String,
    /// `liked` (the LL… likes playlist), `uploads` (the UU… own-uploads
    /// playlist), or `user` (a created playlist).
    pub kind: String,
    /// The API's item count at sync time.
    pub items: u64,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One playlist member, as stored in `youtube/playlist-items/<id>.jsonl`.
/// Keyed by video id (a video appearing twice in one playlist collapses to
/// one row — membership, not ordering, is what the history tracks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PlaylistItemRow {
    pub account: String,
    pub playlist_id: String,
    pub video_id: String,
    pub title: String,
    /// When the item entered the playlist (for Liked videos: the like time).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub added_at: String,
    /// The video's channel (the API's `videoOwnerChannelTitle`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One line of the append-only `youtube/events/YYYY-MM.jsonl` history
/// stream. `kind` is `subscribed` / `unsubscribed` / `playlist-added` /
/// `playlist-removed` / `item-added` / `item-removed`; only the fields that
/// apply to the kind are present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct YoutubeEvent {
    /// RFC3339. The source's own time for additions when it knows one (so
    /// the first sync backfills a real timeline); the sync time for
    /// removals (the true time isn't observable).
    pub ts: String,
    pub account: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub playlist_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub playlist_title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub playlist_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel_title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_title: String,
}

/// A playlist still to be walked in the current pass.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PendingPlaylist {
    pub id: String,
    pub title: String,
    pub kind: String,
}

/// Cursors and staged rows of an in-progress pass, persisted after every
/// page so a budget-capped or interrupted pass resumes exactly where it
/// stopped — finished phases are never re-fetched, and membership is only
/// diffed once a phase's walk is complete (a partial fetch must never
/// masquerade as removals).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PassProgress {
    /// Phase 1 — `subscriptions.list` paging.
    #[serde(default)]
    pub subs_done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subs_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_subs: Vec<SubscriptionRow>,
    /// Phase 2a — the channel's `relatedPlaylists` (likes + uploads) have
    /// been resolved and staged.
    #[serde(default)]
    pub special_fetched: bool,
    /// Phase 2b — `playlists.list?mine=true` paging.
    #[serde(default)]
    pub playlists_done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlists_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_playlists: Vec<PlaylistRow>,
    /// Phase 3 — playlists whose item walk hasn't finished, front first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<PendingPlaylist>,
    /// `playlistItems.list` cursor into `pending[0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_items: Vec<PlaylistItemRow>,
    /// Items committed so far this pass (becomes the account's item count).
    #[serde(default)]
    pub items_total: u64,
}

/// Per-account sync progress, persisted in `.trove/youtube-sync.json`
/// (keyed by Google `sub`). Deleting an account's entry only forgets
/// counters and cursors — the diff baseline lives in the snapshots, so
/// history is never duplicated.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct YoutubeAccountState {
    /// Display address (for the index / UI; the map key is the `sub`).
    #[serde(default)]
    pub email: String,
    /// Current subscription count (as of the last completed phase).
    #[serde(default)]
    pub subscriptions: u64,
    /// Current playlist count (liked + uploads + user).
    #[serde(default)]
    pub playlists: u64,
    /// Items across all playlists at the last completed pass.
    #[serde(default)]
    pub items: u64,
    /// History events ever appended for this account.
    #[serde(default)]
    pub events: u64,
    /// RFC3339 local time of the last *completed* pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,
    /// Why this account's last pass failed, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The in-progress pass, when one was cut short by the budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<PassProgress>,
}

/// The whole YouTube sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct YoutubeSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Per-account progress, keyed by Google `sub`.
    pub accounts: BTreeMap<String, YoutubeAccountState>,
}

/// Result of one YouTube sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct YoutubeSyncStats {
    /// Accounts that produced at least one history event this pass.
    pub accounts: u32,
    /// History events appended this pass.
    pub events: u64,
    /// Every connected account finished its pass (no budget cut-off).
    pub complete: bool,
}

/// Status-level fetch errors needing distinct handling. YouTube signals
/// quota exhaustion as HTTP 403 (`quotaExceeded`), so 403 and 429 share the
/// retry-next-pass arm.
#[derive(Debug)]
enum FetchError {
    RateLimited,
    Unauthorized,
    /// A playlist that vanished or is hidden — skip it, never an error.
    NotFound,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited or out of quota (HTTP 403/429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Thin YouTube Data API v3 client. The base URL is injected so the sync
/// orchestration is testable against a local stub (the
/// `oura.rs`/`tasks.rs`/`gmail.rs` pattern).
struct YoutubeClient {
    base: String,
    token: String,
}

impl YoutubeClient {
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
            Err(ureq::Error::Status(403, _)) => Err(FetchError::RateLimited),
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

    /// One page of a list endpoint: the `items` array plus `nextPageToken`.
    fn page(
        &self,
        path: &str,
        mut params: Vec<(&'static str, String)>,
        page_token: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>), FetchError> {
        params.push(("maxResults", PAGE_SIZE.to_string()));
        if let Some(t) = page_token {
            params.push(("pageToken", t.to_string()));
        }
        let v = self.get(path, &params)?;
        let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let next = v.get("nextPageToken").and_then(Value::as_str).map(str::to_string);
        Ok((items, next))
    }

    /// One page of the account's subscriptions.
    fn subscriptions_page(&self, token: Option<&str>) -> Result<(Vec<Value>, Option<String>), FetchError> {
        self.page(
            "/youtube/v3/subscriptions",
            vec![("part", "snippet,contentDetails".into()), ("mine", "true".into())],
            token,
        )
    }

    /// The account's `relatedPlaylists`: (likes id, uploads id), either of
    /// which may be empty.
    fn related_playlists(&self) -> Result<(String, String), FetchError> {
        let v = self.get(
            "/youtube/v3/channels",
            &[("part", "contentDetails".into()), ("mine", "true".into())],
        )?;
        let rp = v
            .get("items")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("contentDetails"))
            .and_then(|c| c.get("relatedPlaylists"))
            .cloned()
            .unwrap_or_default();
        let take = |k: &str| rp.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        Ok((take("likes"), take("uploads")))
    }

    /// Metadata for specific playlists (the likes/uploads pair) by id.
    fn playlists_by_id(&self, ids: &[String]) -> Result<Vec<Value>, FetchError> {
        let v = self.get(
            "/youtube/v3/playlists",
            &[
                ("part", "snippet,contentDetails".into()),
                ("id", ids.join(",")),
                ("maxResults", PAGE_SIZE.to_string()),
            ],
        )?;
        Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// One page of the account's own (created) playlists.
    fn my_playlists(&self, token: Option<&str>) -> Result<(Vec<Value>, Option<String>), FetchError> {
        self.page(
            "/youtube/v3/playlists",
            vec![("part", "snippet,contentDetails".into()), ("mine", "true".into())],
            token,
        )
    }

    /// One page of a playlist's items. `None` when the playlist 404s
    /// (hidden Liked videos, a channel with no uploads, deleted mid-pass) —
    /// the caller skips it rather than diffing against nothing.
    fn playlist_items(
        &self,
        playlist_id: &str,
        token: Option<&str>,
    ) -> Result<Option<(Vec<Value>, Option<String>)>, FetchError> {
        match self.page(
            "/youtube/v3/playlistItems",
            vec![
                ("part", "snippet,contentDetails".into()),
                ("playlistId", playlist_id.to_string()),
            ],
            token,
        ) {
            Err(FetchError::NotFound) => Ok(None),
            other => other.map(Some),
        }
    }
}

/// Per-pass request allowance. `None` = unlimited (manual pull).
struct Budget(Option<u32>);

impl Budget {
    fn take(&mut self) -> bool {
        match &mut self.0 {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }
}

/// Copy `key` from `src` into `extra` when present and non-empty —
/// full fidelity without writing `""`/`null` fields.
fn extra_insert(extra: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        if !v.is_null() && v.as_str() != Some("") {
            extra.insert(key.to_string(), v.clone());
        }
    }
}

/// API subscription JSON → a normalized row. `None` without a channel id.
fn parse_subscription(v: &Value, account: &str) -> Option<SubscriptionRow> {
    let snippet = v.get("snippet")?;
    let channel_id = snippet
        .get("resourceId")
        .and_then(|r| r.get("channelId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let mut extra = Map::new();
    extra_insert(&mut extra, "description", snippet.get("description"));
    if let Some(cd) = v.get("contentDetails") {
        extra_insert(&mut extra, "totalItemCount", cd.get("totalItemCount"));
        extra_insert(&mut extra, "newItemCount", cd.get("newItemCount"));
    }
    Some(SubscriptionRow {
        account: account.to_string(),
        channel_id,
        title: snippet.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
        subscribed_at: snippet.get("publishedAt").and_then(Value::as_str).unwrap_or_default().to_string(),
        extra,
    })
}

/// API playlist JSON → a normalized row with the given `kind`. `None`
/// without an id.
fn parse_playlist(v: &Value, account: &str, kind: &str) -> Option<PlaylistRow> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let snippet = v.get("snippet").cloned().unwrap_or_default();
    let mut extra = Map::new();
    extra_insert(&mut extra, "description", snippet.get("description"));
    extra_insert(&mut extra, "publishedAt", snippet.get("publishedAt"));
    Some(PlaylistRow {
        account: account.to_string(),
        id,
        title: snippet.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
        kind: kind.to_string(),
        items: v
            .get("contentDetails")
            .and_then(|c| c.get("itemCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        extra,
    })
}

/// API playlistItem JSON → a normalized row. `None` without a video id.
fn parse_playlist_item(v: &Value, account: &str, playlist_id: &str) -> Option<PlaylistItemRow> {
    let snippet = v.get("snippet").cloned().unwrap_or_default();
    let video_id = snippet
        .get("resourceId")
        .and_then(|r| r.get("videoId"))
        .and_then(Value::as_str)
        .or_else(|| v.get("contentDetails").and_then(|c| c.get("videoId")).and_then(Value::as_str))
        .filter(|s| !s.is_empty())?
        .to_string();
    let mut extra = Map::new();
    extra_insert(&mut extra, "videoOwnerChannelId", snippet.get("videoOwnerChannelId"));
    extra_insert(&mut extra, "position", snippet.get("position"));
    if let Some(cd) = v.get("contentDetails") {
        extra_insert(&mut extra, "videoPublishedAt", cd.get("videoPublishedAt"));
    }
    Some(PlaylistItemRow {
        account: account.to_string(),
        playlist_id: playlist_id.to_string(),
        video_id,
        title: snippet.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
        added_at: snippet.get("publishedAt").and_then(Value::as_str).unwrap_or_default().to_string(),
        channel: snippet
            .get("videoOwnerChannelTitle")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        extra,
    })
}

/// The event timestamp: the source's own time when it can name an
/// `events/YYYY-MM` partition, else the sync time — never an unwritable ts.
fn event_ts(preferred: &str, now: &str) -> String {
    if Partition::Month.key(preferred).is_some() {
        preferred.to_string()
    } else {
        now.to_string()
    }
}

/// Membership diff: (in `next` but not `prev`, in `prev` but not `next`),
/// by key. Pure — the testable heart of the history stream.
fn diff_keys<'a, T>(
    prev: &'a [T],
    next: &'a [T],
    key: impl Fn(&T) -> &str,
) -> (Vec<&'a T>, Vec<&'a T>) {
    let pk: HashSet<&str> = prev.iter().map(|t| key(t)).collect();
    let nk: HashSet<&str> = next.iter().map(|t| key(t)).collect();
    let added = next.iter().filter(|t| !pk.contains(key(t))).collect();
    let removed = prev.iter().filter(|t| !nk.contains(key(t))).collect();
    (added, removed)
}

/// Dedupe staged rows by key into a sorted Vec (first occurrence wins —
/// specials are staged before `mine=true` pages, so their kind sticks).
fn dedupe_sorted<T: Clone>(rows: &[T], key: impl Fn(&T) -> &str) -> Vec<T> {
    let mut by_key: BTreeMap<String, T> = BTreeMap::new();
    for r in rows {
        by_key.entry(key(r).to_string()).or_insert_with(|| r.clone());
    }
    by_key.into_values().collect()
}

/// The snapshot file for one playlist's membership. Playlist ids are
/// `[A-Za-z0-9_-]`; anything else is defensively mapped to `_` (and
/// [`Vault::resolve`] still guards the path).
fn playlist_items_file(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .collect();
    format!("{ITEMS_DIR}/{safe}.jsonl")
}

impl Vault {
    /// One YouTube sync pass across every connected Google account: refresh
    /// each account's token, then resume or start its phase walk
    /// (subscriptions → playlist catalog → per-playlist membership). A
    /// silent no-op when no Google account is connected. Per-account
    /// failures are recorded in `.trove/youtube-sync.json` and never abort
    /// other accounts — the next pass resumes from the persisted cursors.
    pub fn collect_youtube(&self, budget: Option<u32>) -> Result<YoutubeSyncStats> {
        let accounts = self.google_status()?.accounts;
        let mut state = self.read_youtube_sync().unwrap_or_default();
        // Forget counters for accounts that have been disconnected (their
        // vault data stays — raw data is never deleted by a sync).
        let live: HashSet<&str> = accounts.iter().map(|a| a.sub.as_str()).collect();
        state.accounts.retain(|sub, _| live.contains(sub.as_str()));

        if accounts.is_empty() {
            // Persist the pruning above so a disconnected account's row
            // doesn't linger in the state or the index.
            if self.resolve(SYNC_FILE).map(|p| p.exists()).unwrap_or(false) {
                state.updated = Local::now().to_rfc3339();
                self.write_youtube_sync(&state)?;
                self.write_youtube_index(&state)?;
            }
            return Ok(YoutubeSyncStats::default());
        }

        let now = Local::now().to_rfc3339();
        let mut budget = Budget(budget);
        let mut stats = YoutubeSyncStats::default();

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
            let client = YoutubeClient { base: YOUTUBE_API.to_string(), token };
            let before = {
                let astate = state.accounts.entry(acct.sub.clone()).or_default();
                astate.email = acct.email.clone();
                astate.error = None;
                astate.events
            };
            if let Err(e) =
                self.youtube_sync_account(&client, &acct.sub, &acct.email, &now, &mut state, &mut budget)
            {
                let msg = format!("{}", status_error(&acct.email, e));
                let astate = state.accounts.entry(acct.sub.clone()).or_default();
                astate.error = Some(msg);
            }
            let after = state.accounts.get(&acct.sub).map(|s| s.events).unwrap_or(before);
            if after > before {
                stats.accounts += 1;
                stats.events += after - before;
            }
        }

        stats.complete = state.accounts.values().all(|s| s.progress.is_none());
        state.updated = Local::now().to_rfc3339();
        self.write_youtube_sync(&state)?;
        self.write_youtube_index(&state)?;
        Ok(stats)
    }

    /// Resume (or start) one account's phase walk. Cursors and staged rows
    /// persist after every page; each phase commits (diff → events →
    /// snapshot) only from a complete walk.
    fn youtube_sync_account(
        &self,
        client: &YoutubeClient,
        sub: &str,
        email: &str,
        now: &str,
        state: &mut YoutubeSyncState,
        budget: &mut Budget,
    ) -> Result<(), FetchError> {
        let mut prog = state.accounts.get(sub).and_then(|s| s.progress.clone()).unwrap_or_default();

        // Phase 1 — subscriptions.
        while !prog.subs_done {
            if !budget.take() {
                return self.persist_progress(state, sub, &prog);
            }
            let (vals, next) = client.subscriptions_page(prog.subs_token.as_deref())?;
            prog.staged_subs.extend(vals.iter().filter_map(|v| parse_subscription(v, email)));
            match next {
                Some(t) => prog.subs_token = Some(t),
                None => {
                    let (events, count) =
                        self.youtube_commit_subscriptions(email, &prog.staged_subs, now).map_err(soft)?;
                    let astate = state.accounts.entry(sub.to_string()).or_default();
                    astate.subscriptions = count;
                    astate.events += events;
                    prog.subs_done = true;
                    prog.subs_token = None;
                    prog.staged_subs.clear();
                }
            }
            self.persist_progress(state, sub, &prog)?;
        }

        // Phase 2a — the likes + uploads playlists off the channel record.
        // Both calls happen together so a budget cut between them can't
        // half-stage the pair; the worst case re-spends one channels.list.
        if !prog.special_fetched {
            if !budget.take() {
                return self.persist_progress(state, sub, &prog);
            }
            let (likes, uploads) = client.related_playlists()?;
            let specials: Vec<(String, &str)> = [(likes, "liked"), (uploads, "uploads")]
                .into_iter()
                .filter(|(id, _)| !id.is_empty())
                .collect();
            if !specials.is_empty() {
                if !budget.take() {
                    return self.persist_progress(state, sub, &prog);
                }
                let ids: Vec<String> = specials.iter().map(|(id, _)| id.clone()).collect();
                for v in client.playlists_by_id(&ids)? {
                    let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
                    let kind = specials
                        .iter()
                        .find(|(sid, _)| sid == id)
                        .map(|(_, k)| *k)
                        .unwrap_or("user");
                    if let Some(row) = parse_playlist(&v, email, kind) {
                        prog.staged_playlists.push(row);
                    }
                }
            }
            prog.special_fetched = true;
            self.persist_progress(state, sub, &prog)?;
        }

        // Phase 2b — the user's created playlists.
        while !prog.playlists_done {
            if !budget.take() {
                return self.persist_progress(state, sub, &prog);
            }
            let (vals, next) = client.my_playlists(prog.playlists_token.as_deref())?;
            prog.staged_playlists.extend(vals.iter().filter_map(|v| parse_playlist(v, email, "user")));
            match next {
                Some(t) => prog.playlists_token = Some(t),
                None => {
                    let (events, committed) =
                        self.youtube_commit_playlists(email, &prog.staged_playlists, now).map_err(soft)?;
                    let astate = state.accounts.entry(sub.to_string()).or_default();
                    astate.playlists = committed.len() as u64;
                    astate.events += events;
                    prog.playlists_done = true;
                    prog.playlists_token = None;
                    prog.staged_playlists.clear();
                    prog.pending = committed;
                }
            }
            self.persist_progress(state, sub, &prog)?;
        }

        // Phase 3 — each playlist's membership.
        while let Some(p) = prog.pending.first().cloned() {
            if !budget.take() {
                return self.persist_progress(state, sub, &prog);
            }
            match client.playlist_items(&p.id, prog.items_token.as_deref())? {
                // 404 — hidden or deleted mid-pass: skip without diffing,
                // so an unreadable playlist can't fake a mass removal.
                None => {
                    prog.pending.remove(0);
                    prog.items_token = None;
                    prog.staged_items.clear();
                }
                Some((vals, next)) => {
                    prog.staged_items
                        .extend(vals.iter().filter_map(|v| parse_playlist_item(v, email, &p.id)));
                    match next {
                        Some(t) => prog.items_token = Some(t),
                        None => {
                            let (events, count) = self
                                .youtube_commit_playlist_items(&p, &prog.staged_items, now)
                                .map_err(soft)?;
                            let astate = state.accounts.entry(sub.to_string()).or_default();
                            astate.events += events;
                            prog.items_total += count;
                            prog.pending.remove(0);
                            prog.items_token = None;
                            prog.staged_items.clear();
                        }
                    }
                }
            }
            self.persist_progress(state, sub, &prog)?;
        }

        // Pass complete — the next pass starts fresh from the snapshots.
        let astate = state.accounts.entry(sub.to_string()).or_default();
        astate.items = prog.items_total;
        astate.last_sync = Some(now.to_string());
        astate.progress = None;
        self.write_youtube_sync(state).map_err(soft)
    }

    /// Persist the in-progress pass — called after every page, which is
    /// what makes a budget-capped pass resumable.
    fn persist_progress(
        &self,
        state: &mut YoutubeSyncState,
        sub: &str,
        prog: &PassProgress,
    ) -> Result<(), FetchError> {
        state.accounts.entry(sub.to_string()).or_default().progress = Some(prog.clone());
        self.write_youtube_sync(state).map_err(soft)
    }

    /// Commit a complete subscription walk: diff against this account's
    /// previous snapshot rows, append `subscribed`/`unsubscribed` history,
    /// rewrite the merged snapshot. Returns (events appended, row count).
    fn youtube_commit_subscriptions(
        &self,
        email: &str,
        staged: &[SubscriptionRow],
        now: &str,
    ) -> Result<(u64, u64)> {
        let prev_all: Vec<SubscriptionRow> = self.read_snapshot(SUBS_FILE)?;
        let (prev_mine, mut merged): (Vec<_>, Vec<_>) =
            prev_all.into_iter().partition(|r| r.account == email);
        let next = dedupe_sorted(staged, |r| &r.channel_id);
        let (added, removed) = diff_keys(&prev_mine, &next, |r| &r.channel_id);
        let mut events = Vec::new();
        for r in added {
            events.push(YoutubeEvent {
                ts: event_ts(&r.subscribed_at, now),
                account: email.to_string(),
                kind: "subscribed".into(),
                channel_id: r.channel_id.clone(),
                channel_title: r.title.clone(),
                ..Default::default()
            });
        }
        for r in removed {
            events.push(YoutubeEvent {
                ts: now.to_string(),
                account: email.to_string(),
                kind: "unsubscribed".into(),
                channel_id: r.channel_id.clone(),
                channel_title: r.title.clone(),
                ..Default::default()
            });
        }
        self.append_youtube_events(&events)?;
        let count = next.len() as u64;
        merged.extend(next);
        merged.sort_by(|a, b| {
            (a.account.as_str(), a.channel_id.as_str()).cmp(&(b.account.as_str(), b.channel_id.as_str()))
        });
        self.write_snapshot(SUBS_FILE, &merged)?;
        Ok((events.len() as u64, count))
    }

    /// Commit a complete playlist-catalog walk: diff, append
    /// `playlist-added`/`playlist-removed` history, rewrite the merged
    /// snapshot. Returns (events appended, the committed set as the
    /// pending-walk queue). A removed playlist's `playlist-items/` snapshot
    /// is deliberately left in place — history stays complete.
    fn youtube_commit_playlists(
        &self,
        email: &str,
        staged: &[PlaylistRow],
        now: &str,
    ) -> Result<(u64, Vec<PendingPlaylist>)> {
        let prev_all: Vec<PlaylistRow> = self.read_snapshot(PLAYLISTS_FILE)?;
        let (prev_mine, mut merged): (Vec<_>, Vec<_>) =
            prev_all.into_iter().partition(|r| r.account == email);
        let next = dedupe_sorted(staged, |r| &r.id);
        let (added, removed) = diff_keys(&prev_mine, &next, |r| &r.id);
        let mut events = Vec::new();
        for r in added {
            events.push(YoutubeEvent {
                ts: now.to_string(),
                account: email.to_string(),
                kind: "playlist-added".into(),
                playlist_id: r.id.clone(),
                playlist_title: r.title.clone(),
                playlist_kind: r.kind.clone(),
                ..Default::default()
            });
        }
        for r in removed {
            events.push(YoutubeEvent {
                ts: now.to_string(),
                account: email.to_string(),
                kind: "playlist-removed".into(),
                playlist_id: r.id.clone(),
                playlist_title: r.title.clone(),
                playlist_kind: r.kind.clone(),
                ..Default::default()
            });
        }
        self.append_youtube_events(&events)?;
        let pending: Vec<PendingPlaylist> = next
            .iter()
            .map(|r| PendingPlaylist { id: r.id.clone(), title: r.title.clone(), kind: r.kind.clone() })
            .collect();
        merged.extend(next);
        merged.sort_by(|a, b| (a.account.as_str(), a.id.as_str()).cmp(&(b.account.as_str(), b.id.as_str())));
        self.write_snapshot(PLAYLISTS_FILE, &merged)?;
        Ok((events.len() as u64, pending))
    }

    /// Commit one playlist's complete item walk: diff membership by video
    /// id against the previous snapshot, append `item-added`/`item-removed`
    /// history (additions stamped with the source's added-at time — the
    /// like time, for Liked videos), rewrite the snapshot. Returns
    /// (events appended, current item count).
    fn youtube_commit_playlist_items(
        &self,
        playlist: &PendingPlaylist,
        staged: &[PlaylistItemRow],
        now: &str,
    ) -> Result<(u64, u64)> {
        let rel = playlist_items_file(&playlist.id);
        let prev: Vec<PlaylistItemRow> = self.read_snapshot(&rel)?;
        let next = dedupe_sorted(staged, |r| &r.video_id);
        let (added, removed) = diff_keys(&prev, &next, |r| &r.video_id);
        let mut events = Vec::new();
        for r in added {
            events.push(YoutubeEvent {
                ts: event_ts(&r.added_at, now),
                account: r.account.clone(),
                kind: "item-added".into(),
                playlist_id: playlist.id.clone(),
                playlist_title: playlist.title.clone(),
                playlist_kind: playlist.kind.clone(),
                channel_title: r.channel.clone(),
                video_id: r.video_id.clone(),
                video_title: r.title.clone(),
                ..Default::default()
            });
        }
        for r in removed {
            events.push(YoutubeEvent {
                ts: now.to_string(),
                account: r.account.clone(),
                kind: "item-removed".into(),
                playlist_id: playlist.id.clone(),
                playlist_title: playlist.title.clone(),
                playlist_kind: playlist.kind.clone(),
                channel_title: r.channel.clone(),
                video_id: r.video_id.clone(),
                video_title: r.title.clone(),
                ..Default::default()
            });
        }
        self.append_youtube_events(&events)?;
        let count = next.len() as u64;
        self.write_snapshot(&rel, &next)?;
        Ok((events.len() as u64, count))
    }

    /// Append history events to their month's partition.
    fn append_youtube_events(&self, events: &[YoutubeEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.stream(EVENTS_DIR, Partition::Month).append(events, |e| &e.ts)
    }

    /// The persisted YouTube sync progress, if a sync has ever run.
    pub fn read_youtube_sync(&self) -> Option<YoutubeSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Atomic write so readers never see a torn file — called after every
    /// page, which is what makes the pass resumable.
    fn write_youtube_sync(&self, state: &YoutubeSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Regenerate the human-readable summary at `youtube/index.md`.
    fn write_youtube_index(&self, state: &YoutubeSyncState) -> Result<()> {
        let mut md = format!(
            "# YouTube\n\nLast sync: {}\n\n| Account | Subscriptions | Playlists | Playlist items | History events | Pass | Last full pass | Error |\n|---|---|---|---|---|---|---|---|\n",
            state.updated
        );
        for s in state.accounts.values() {
            let pass = if s.progress.is_some() {
                "in progress…"
            } else if s.last_sync.is_some() {
                "complete"
            } else {
                "—"
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {pass} | {} | {} |\n",
                s.email,
                s.subscriptions,
                s.playlists,
                s.items,
                s.events,
                s.last_sync.as_deref().unwrap_or("—"),
                s.error.as_deref().unwrap_or(""),
            ));
        }
        crate::store::write_atomic(&self.resolve(INDEX_FILE)?, md.as_bytes())
    }

    /// Pull every connected account's YouTube library into the vault,
    /// unbudgeted — the manual "Sync now" path that drains even a huge
    /// Liked-videos backfill in one go. Blocking (network).
    pub fn youtube_pull(&self) -> Result<YoutubeSyncStats> {
        if self.google_status()?.accounts.is_empty() {
            bail!("no Google account is connected");
        }
        self.collect_youtube(None)
    }
}

/// A vault write error inside the API-fetch path → a soft `FetchError`.
fn soft(e: anyhow::Error) -> FetchError {
    FetchError::Other(format!("{e:#}"))
}

/// Map a status-level YouTube error to a user-facing message for the sync
/// log.
fn status_error(account: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::RateLimited => {
            anyhow!("YouTube rate limited the sync or the daily quota ran out ({account}) — it resumes next pass")
        }
        FetchError::Unauthorized => {
            anyhow!("YouTube rejected the token ({account}, 401) — reconnect from the Integrations tab")
        }
        FetchError::NotFound => anyhow!("youtube {account}: not found (HTTP 404)"),
        FetchError::Other(m) => anyhow!("youtube {account}: {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-youtube-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    const NOW: &str = "2026-06-12T10:00:00-07:00";

    fn item_row(video_id: &str, title: &str, added_at: &str) -> PlaylistItemRow {
        PlaylistItemRow {
            account: "me@gmail.com".into(),
            playlist_id: "LLx".into(),
            video_id: video_id.into(),
            title: title.into(),
            added_at: added_at.into(),
            channel: "Some Channel".into(),
            extra: Map::new(),
        }
    }

    fn read_events(v: &Vault, month: &str) -> Vec<YoutubeEvent> {
        v.stream(EVENTS_DIR, Partition::Month).read(month).unwrap()
    }

    // -- normalization --------------------------------------------------

    #[test]
    fn parses_subscription_playlist_and_item_json() {
        let sub = json!({
            "kind": "youtube#subscription",
            "snippet": {
                "publishedAt": "2023-04-01T17:00:00Z",
                "title": "3Blue1Brown",
                "description": "Math videos",
                "resourceId": { "kind": "youtube#channel", "channelId": "UCYO_abc" }
            },
            "contentDetails": { "totalItemCount": 200, "newItemCount": 3 }
        });
        let s = parse_subscription(&sub, "me@gmail.com").unwrap();
        assert_eq!(s.account, "me@gmail.com");
        assert_eq!(s.channel_id, "UCYO_abc");
        assert_eq!(s.title, "3Blue1Brown");
        assert_eq!(s.subscribed_at, "2023-04-01T17:00:00Z");
        assert_eq!(s.extra["description"], "Math videos");
        assert_eq!(s.extra["totalItemCount"], 200);

        let pl = json!({
            "id": "PLcook",
            "snippet": { "publishedAt": "2022-01-05T00:00:00Z", "title": "Cooking", "description": "" },
            "contentDetails": { "itemCount": 12 }
        });
        let p = parse_playlist(&pl, "me@gmail.com", "user").unwrap();
        assert_eq!(p.id, "PLcook");
        assert_eq!(p.kind, "user");
        assert_eq!(p.items, 12);
        assert!(!p.extra.contains_key("description"), "empty fields omitted");
        assert_eq!(p.extra["publishedAt"], "2022-01-05T00:00:00Z");

        let it = json!({
            "snippet": {
                "publishedAt": "2024-03-05T10:00:00Z",
                "title": "Some video",
                "videoOwnerChannelTitle": "Chef",
                "videoOwnerChannelId": "UCchef",
                "position": 0,
                "resourceId": { "videoId": "dQw4w9WgXcQ" }
            },
            "contentDetails": { "videoId": "dQw4w9WgXcQ", "videoPublishedAt": "2024-03-01T00:00:00Z" }
        });
        let i = parse_playlist_item(&it, "me@gmail.com", "LLx").unwrap();
        assert_eq!(i.video_id, "dQw4w9WgXcQ");
        assert_eq!(i.playlist_id, "LLx");
        assert_eq!(i.added_at, "2024-03-05T10:00:00Z");
        assert_eq!(i.channel, "Chef");
        assert_eq!(i.extra["videoOwnerChannelId"], "UCchef");
        assert_eq!(i.extra["videoPublishedAt"], "2024-03-01T00:00:00Z");

        // No id → no row, never a panic.
        assert!(parse_subscription(&json!({"snippet": {}}), "a").is_none());
        assert!(parse_playlist(&json!({"snippet": {}}), "a", "user").is_none());
        assert!(parse_playlist_item(&json!({"snippet": {}}), "a", "p").is_none());
    }

    #[test]
    fn event_ts_prefers_source_time_and_falls_back() {
        assert_eq!(event_ts("2024-03-05T10:00:00Z", NOW), "2024-03-05T10:00:00Z");
        assert_eq!(event_ts("", NOW), NOW, "missing → sync time");
        assert_eq!(event_ts("garbage", NOW), NOW, "unpartitionable → sync time");
    }

    // -- membership diff → history --------------------------------------

    #[test]
    fn membership_diff_keeps_unliked_videos_in_history() {
        let v = temp_vault("diff");
        let p = PendingPlaylist { id: "LLx".into(), title: "Liked videos".into(), kind: "liked".into() };

        // First sync: two liked videos, with historic like times.
        let first = [
            item_row("vid-b", "Newer like", "2026-06-01T08:00:00Z"),
            item_row("vid-a", "Old like", "2024-03-05T10:00:00Z"),
        ];
        let (events, count) = v.youtube_commit_playlist_items(&p, &first, NOW).unwrap();
        assert_eq!((events, count), (2, 2));
        // The like timeline is materialized into the right historic months.
        let mar = read_events(&v, "2024-03");
        assert_eq!(mar.len(), 1);
        assert_eq!(mar[0].kind, "item-added");
        assert_eq!(mar[0].video_id, "vid-a");
        assert_eq!(mar[0].ts, "2024-03-05T10:00:00Z");
        assert_eq!(mar[0].playlist_kind, "liked");
        assert_eq!(read_events(&v, "2026-06").len(), 1);
        // Snapshot holds both, sorted by video id.
        let snap: Vec<PlaylistItemRow> = v.read_snapshot(&playlist_items_file("LLx")).unwrap();
        assert_eq!(snap.iter().map(|r| r.video_id.as_str()).collect::<Vec<_>>(), ["vid-a", "vid-b"]);

        // Second sync: vid-a was unliked, vid-c (no added-at) was liked.
        let second = [item_row("vid-b", "Newer like", "2026-06-01T08:00:00Z"), item_row("vid-c", "Fresh", "")];
        let (events, count) = v.youtube_commit_playlist_items(&p, &second, NOW).unwrap();
        assert_eq!((events, count), (2, 2));
        let june: Vec<YoutubeEvent> = read_events(&v, "2026-06");
        let removed: Vec<_> = june.iter().filter(|e| e.kind == "item-removed").collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].video_id, "vid-a");
        assert_eq!(removed[0].video_title, "Old like", "the unliked video survives in history");
        assert_eq!(removed[0].ts, NOW, "removal time isn't observable — stamped at sync");
        assert!(june.iter().any(|e| e.kind == "item-added" && e.video_id == "vid-c" && e.ts == NOW));
        // Snapshot is the current library only.
        let snap: Vec<PlaylistItemRow> = v.read_snapshot(&playlist_items_file("LLx")).unwrap();
        assert_eq!(snap.iter().map(|r| r.video_id.as_str()).collect::<Vec<_>>(), ["vid-b", "vid-c"]);

        // Unchanged third sync: no events, snapshot byte-stable.
        let (events, _) = v.youtube_commit_playlist_items(&p, &second, NOW).unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn subscription_diff_merges_accounts_and_logs_churn() {
        let v = temp_vault("subs");
        let row = |account: &str, id: &str, title: &str| SubscriptionRow {
            account: account.into(),
            channel_id: id.into(),
            title: title.into(),
            subscribed_at: "2025-12-01T00:00:00Z".into(),
            extra: Map::new(),
        };
        // Two accounts coexist in one snapshot.
        v.youtube_commit_subscriptions("a@gmail.com", &[row("a@gmail.com", "UC1", "One")], NOW).unwrap();
        let (events, count) = v
            .youtube_commit_subscriptions(
                "b@gmail.com",
                &[row("b@gmail.com", "UC1", "One"), row("b@gmail.com", "UC2", "Two")],
                NOW,
            )
            .unwrap();
        assert_eq!((events, count), (2, 2));
        let snap: Vec<SubscriptionRow> = v.read_snapshot(SUBS_FILE).unwrap();
        assert_eq!(snap.len(), 3, "account b's commit didn't clobber account a");
        assert_eq!(snap[0].account, "a@gmail.com");

        // Account a unsubscribes from UC1 — b's identical channel is untouched.
        let (events, count) = v.youtube_commit_subscriptions("a@gmail.com", &[], NOW).unwrap();
        assert_eq!((events, count), (1, 0));
        let snap: Vec<SubscriptionRow> = v.read_snapshot(SUBS_FILE).unwrap();
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|r| r.account == "b@gmail.com"));
        let june = read_events(&v, "2026-06");
        let unsub: Vec<_> = june.iter().filter(|e| e.kind == "unsubscribed").collect();
        assert_eq!(unsub.len(), 1);
        assert_eq!(unsub[0].account, "a@gmail.com");
        assert_eq!(unsub[0].channel_id, "UC1");
        // The original subscribe times landed in their historic month.
        assert_eq!(read_events(&v, "2025-12").len(), 3);
    }

    // -- state ------------------------------------------------------------

    #[test]
    fn sync_state_round_trips_with_cursors() {
        let v = temp_vault("state");
        assert!(v.read_youtube_sync().is_none());
        let mut state = YoutubeSyncState { updated: NOW.into(), ..Default::default() };
        state.accounts.insert(
            "12345".into(),
            YoutubeAccountState {
                email: "me@gmail.com".into(),
                subscriptions: 120,
                playlists: 7,
                items: 4300,
                events: 4427,
                last_sync: Some(NOW.into()),
                error: None,
                progress: Some(PassProgress {
                    subs_done: true,
                    pending: vec![PendingPlaylist { id: "LLx".into(), title: "Liked videos".into(), kind: "liked".into() }],
                    items_token: Some("page-9".into()),
                    staged_items: vec![item_row("vid-a", "Old like", "2024-03-05T10:00:00Z")],
                    items_total: 450,
                    special_fetched: true,
                    playlists_done: true,
                    ..Default::default()
                }),
            },
        );
        v.write_youtube_sync(&state).unwrap();
        let loaded = v.read_youtube_sync().unwrap();
        let a = &loaded.accounts["12345"];
        assert_eq!(a.email, "me@gmail.com");
        assert_eq!(a.subscriptions, 120);
        let prog = a.progress.as_ref().unwrap();
        assert!(prog.subs_done);
        assert_eq!(prog.items_token.as_deref(), Some("page-9"));
        assert_eq!(prog.pending[0].id, "LLx");
        assert_eq!(prog.staged_items[0].video_id, "vid-a");
        assert_eq!(prog.items_total, 450);
    }

    #[test]
    fn collect_without_an_account_is_a_silent_noop() {
        let v = temp_vault("noaccount");
        let stats = v.collect_youtube(Some(10)).unwrap();
        assert_eq!(stats.events, 0);
        assert!(v.read_youtube_sync().is_none());
        assert!(!v.root().join(INDEX_FILE).exists());
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
    fn index_lists_accounts() {
        let v = temp_vault("index");
        let mut state = YoutubeSyncState { updated: NOW.into(), ..Default::default() };
        state.accounts.insert(
            "1".into(),
            YoutubeAccountState {
                email: "a@gmail.com".into(),
                subscriptions: 12,
                playlists: 3,
                items: 240,
                events: 255,
                last_sync: Some(NOW.into()),
                ..Default::default()
            },
        );
        v.write_youtube_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join(INDEX_FILE)).unwrap();
        assert!(md.contains(&format!("| a@gmail.com | 12 | 3 | 240 | 255 | complete | {NOW} |")), "{md}");
    }

    #[test]
    fn budget_counts_down_and_unlimited_never_blocks() {
        let mut b = Budget(Some(2));
        assert!(b.take());
        assert!(b.take());
        assert!(!b.take());
        let mut u = Budget(None);
        for _ in 0..1000 {
            assert!(u.take());
        }
    }

    // -- full pass against a local stub, with a budget interruption -------

    /// Minimal HTTP/1.1 stub: routes the request target to a canned JSON
    /// body and logs every target hit. The listener thread outlives the
    /// test harmlessly.
    fn stub_server(route: impl Fn(&str) -> String + Send + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            head.extend_from_slice(&buf[..n]);
                            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let req = String::from_utf8_lossy(&head);
                let target = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                log2.lock().unwrap().push(target.clone());
                let body = route(&target);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (base, log)
    }

    fn fixture_route(target: &str) -> String {
        let sub = |id: &str, title: &str| {
            json!({"snippet": {"publishedAt": "2025-12-01T00:00:00Z", "title": title,
                   "resourceId": {"channelId": id}}})
        };
        let item = |vid: &str, title: &str, at: &str| {
            json!({"snippet": {"publishedAt": at, "title": title, "videoOwnerChannelTitle": "Owner",
                   "resourceId": {"videoId": vid}}})
        };
        if target.starts_with("/youtube/v3/subscriptions") {
            if target.contains("pageToken=s2") {
                return json!({"items": [sub("UC2", "Two")]}).to_string();
            }
            return json!({"items": [sub("UC1", "One")], "nextPageToken": "s2"}).to_string();
        }
        if target.starts_with("/youtube/v3/channels") {
            return json!({"items": [{"contentDetails": {"relatedPlaylists": {"likes": "LLx", "uploads": "UUx"}}}]})
                .to_string();
        }
        if target.starts_with("/youtube/v3/playlists") {
            if target.contains("id=") {
                return json!({"items": [
                    {"id": "LLx", "snippet": {"title": "Liked videos"}, "contentDetails": {"itemCount": 2}},
                    {"id": "UUx", "snippet": {"title": "Uploads"}, "contentDetails": {"itemCount": 1}}
                ]})
                .to_string();
            }
            return json!({"items": [
                {"id": "PLuser", "snippet": {"title": "Cooking"}, "contentDetails": {"itemCount": 1}}
            ]})
            .to_string();
        }
        if target.starts_with("/youtube/v3/playlistItems") {
            if target.contains("playlistId=LLx") {
                return json!({"items": [
                    item("vid-a", "Old like", "2024-03-05T10:00:00Z"),
                    item("vid-b", "New like", "2026-06-01T08:00:00Z")
                ]})
                .to_string();
            }
            if target.contains("playlistId=UUx") {
                return json!({"items": [item("vid-u", "My upload", "2025-01-01T00:00:00Z")]}).to_string();
            }
            return json!({"items": [item("vid-c", "Saved", "2026-02-02T00:00:00Z")]}).to_string();
        }
        json!({}).to_string()
    }

    #[test]
    fn budget_interrupted_pass_resumes_from_cursors() {
        let v = temp_vault("resume");
        let (base, log) = stub_server(fixture_route);
        let client = YoutubeClient { base, token: "test-token".into() };
        let mut state = YoutubeSyncState::default();

        // Pass 1, budget 2: exactly the two subscription pages — the pass is
        // cut off before the channel lookup.
        let mut budget = Budget(Some(2));
        v.youtube_sync_account(&client, "sub1", "me@gmail.com", NOW, &mut state, &mut budget)
            .unwrap();
        let persisted = v.read_youtube_sync().unwrap();
        let a = &persisted.accounts["sub1"];
        assert_eq!(a.subscriptions, 2, "subscription phase committed");
        let prog = a.progress.as_ref().expect("interrupted pass persisted");
        assert!(prog.subs_done);
        assert!(!prog.special_fetched, "cut off before phase 2");
        assert!(a.last_sync.is_none(), "pass not complete yet");
        let snap: Vec<SubscriptionRow> = v.read_snapshot(SUBS_FILE).unwrap();
        assert_eq!(snap.len(), 2);
        assert_eq!(log.lock().unwrap().len(), 2);

        // Pass 2, unbudgeted: resumes — subscriptions are NOT re-fetched.
        let mut state = v.read_youtube_sync().unwrap();
        let mut budget = Budget(None);
        v.youtube_sync_account(&client, "sub1", "me@gmail.com", NOW, &mut state, &mut budget)
            .unwrap();
        let hits = log.lock().unwrap().clone();
        assert_eq!(
            hits.iter().filter(|t| t.starts_with("/youtube/v3/subscriptions")).count(),
            2,
            "finished phases never re-fetch: {hits:?}"
        );

        let done = v.read_youtube_sync().unwrap();
        let a = &done.accounts["sub1"];
        assert!(a.progress.is_none(), "pass complete");
        assert_eq!(a.last_sync.as_deref(), Some(NOW));
        assert_eq!(a.playlists, 3, "liked + uploads + user");
        assert_eq!(a.items, 4);
        // 2 subscribed + 3 playlist-added + 4 item-added.
        assert_eq!(a.events, 9);

        let playlists: Vec<PlaylistRow> = v.read_snapshot(PLAYLISTS_FILE).unwrap();
        let kinds: Vec<&str> = playlists.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(playlists.len(), 3);
        assert!(kinds.contains(&"liked") && kinds.contains(&"uploads") && kinds.contains(&"user"));

        let liked: Vec<PlaylistItemRow> = v.read_snapshot(&playlist_items_file("LLx")).unwrap();
        assert_eq!(liked.len(), 2);
        // The like-history timeline landed in its historic months.
        let mar: Vec<YoutubeEvent> = read_events(&v, "2024-03");
        assert_eq!(mar.len(), 1);
        assert_eq!(mar[0].video_id, "vid-a");
        assert_eq!(mar[0].playlist_kind, "liked");

        // Pass 3, steady state: a full re-walk produces zero new events and
        // leaves history untouched (the snapshot is the diff baseline).
        let mut state = v.read_youtube_sync().unwrap();
        let before = state.accounts["sub1"].events;
        let mut budget = Budget(None);
        v.youtube_sync_account(&client, "sub1", "me@gmail.com", NOW, &mut state, &mut budget)
            .unwrap();
        assert_eq!(v.read_youtube_sync().unwrap().accounts["sub1"].events, before);
    }
}
