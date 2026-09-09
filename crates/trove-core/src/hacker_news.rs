//! Hacker News — Periodic sync of submitted stories, comments, and (when the
//! profile is public) favorited items, via the keyless official Firebase API.
//! Favorites are scraped from the public favorites page (HTML — no API endpoint
//! exists). Catalogued in the Phase 2 pass; brief: docs/integrations/hacker-news.md.
//!
//! ## Layers
//!
//! - **Submissions/comments** (stories + comment items from `submitted[]`):
//!   - Raw: `social/hacker-news/raw/YYYY-MM.jsonl` — resolved item object verbatim.
//!   - Contract: `social/hacker-news/YYYY-MM.jsonl` — one [`crate::social::Post`]
//!     per item, deduped by HN item id.
//!
//! - **Favorites** (scraped public favorites page):
//!   - Raw only: `social/hacker-news/favorites.jsonl` — one line per item id
//!     (`{id, ts_collected}`) with best-effort resolved metadata; no API endpoint.
//!     Favorites are NOT authored content and NEVER route to the contract stream.
//!
//! ## Auth
//!
//! Keyless and public — no OAuth, no API key. The user supplies their HN username
//! via a [`crate::registry::ConnectionDef`] (TokenPaste). The username is stored
//! under `.trove/sync/hacker-news.json` (the same `access_token` slot lastfm.rs
//! uses).
//!
//! ## Cursor
//!
//! The watermark (`max_id` seen across all items in `submitted[]`) is persisted
//! at `.trove/hacker-news-sync.json`. A first run fetches all submitted ids;
//! subsequent runs only resolve ids strictly greater than `max_id`.
//!
//! ## Favorites scrape
//!
//! Scrapes `news.ycombinator.com/favorites?id={username}` (paginated via
//! `a.morelink` → `?p=N`). Degrades gracefully: a private profile, network error,
//! or any parse failure logs a note and skips favorites without failing the pull.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::social::Post;
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "hacker-news";
/// Contract stream: `social/hacker-news/YYYY-MM.jsonl`.
const DIR: &str = "social/hacker-news";
/// Raw layer: `social/hacker-news/raw/YYYY-MM.jsonl`.
const RAW_DIR: &str = "social/hacker-news/raw";
/// Favorites raw: per-source flat file (not date-partitioned — favorites are a
/// curated set, not an event stream).
const FAV_FILE: &str = "social/hacker-news/favorites.jsonl";
/// Cursor: max submitted item id seen so far.
const SYNC_FILE: &str = ".trove/hacker-news-sync.json";
/// Secret store key — username lives in the `access_token` slot.
const SERVICE: &str = "hacker-news";

/// Firebase base URL (injectable for tests).
const FIREBASE_BASE: &str = "https://hacker-news.firebaseio.com/v0";
/// HN site base for favorites scrape.
const HN_BASE: &str = "https://news.ycombinator.com";

/// Sync items daily-ish (users post infrequently; the pull is cheap after
/// the first backfill).
pub const HN_SYNC_SECS: u64 = 86_400;

/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Polite inter-request delay: ~3 req/s — the HN Firebase API has no published
/// rate limit but is a public shared service.
const REQ_INTERVAL: Duration = Duration::from_millis(333);

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("items").copied().unwrap_or(0);
            Ok(CollectOutcome::note_if(n > 0, || {
                format!("Hacker News synced — {n} new items")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("Hacker News sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "hacker-news",
        name: "Hacker News",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Hacker News submissions and comments via the keyless official \
                      Firebase API into the unified social stream. Favorites are also collected \
                      when your profile is set to public. First sync backfills your full history; \
                      later syncs are incremental.",
        domain: "social",
        vault_path: "social/hacker-news/",
        toggleable: true,
        setup: &[
            "Enter your Hacker News username and connect.",
            "First sync backfills your full submission history. Later syncs are incremental.",
            "Favorites are collected when your HN profile is set to public \
             (Account → Public → checked). Upvoted items are private and cannot be retrieved.",
        ],
        caveats: "Favorites require a public profile — a private profile skips the favorites \
                  slice without failing the pull. Upvoted items are private (not even scrapeable) \
                  and are out of scope. The favorites slice is scraped from the public page; \
                  HTML changes on HN's side can affect it without notice.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(HN_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some(SERVICE),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = HN username; keyless public API, no key needed).

fn def_connect(vault: &Vault, username: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("username is empty");
    }
    // Best-effort validation: fetch the user profile. A 404-body (null) means
    // the user doesn't exist. Network errors don't block storing the username.
    match fetch_user_profile(FIREBASE_BASE, username) {
        Ok(None) => {
            bail!("Hacker News user {username:?} not found — check the spelling");
        }
        Ok(Some(_)) | Err(_) => {} // found, or couldn't verify — store it anyway
    }
    let token = crate::sync::oauth::TokenSet {
        access_token: username.to_string(),
        refresh_token: None,
        token_type: None,
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &token)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let username = token.access_token;
        if !username.is_empty() {
            accounts.push(ConnectedAccount {
                key: SERVICE.to_string(),
                label: username,
                connected_at: None,
                expires_at: None,
                needs_reconnect: false,
                extra: BTreeMap::new(),
            });
        }
    }
    Ok(ConnectStatus { configured: !accounts.is_empty(), accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: SERVICE,
    display_name: "Hacker News",
    methods: &[ConnectMethod::TokenPaste {
        label: "Hacker News username",
        help: "Your public HN username — submissions and comments are read from your public \
               profile. Enable 'Public' in your HN profile settings to also collect favorites.",
        placeholder: "pg",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["hacker-news"],
    setup: &[
        "Enter your Hacker News username and connect.",
        "Enable 'Public' in your HN profile to also sync your favorites list.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max HN item id ever written across all submitted items. The next
    /// incremental run only resolves ids strictly greater than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_id: Option<u64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_hn_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_hn_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — trait-injectable so tests run offline.

pub(crate) trait HnApi {
    /// `GET /v0/user/{id}.json` — returns the parsed body, or None if 404/null.
    fn user(&self, username: &str) -> Result<Option<Value>>;
    /// `GET /v0/item/{id}.json` — returns the parsed body, or None if 404/null.
    fn item(&self, id: u64) -> Result<Option<Value>>;
    /// Fetch a favorites HTML page. Returns the HTML string.
    fn favorites_page(&self, username: &str, page: u32) -> Result<String>;
}

struct HnClient {
    base: String,
    hn_base: String,
}

impl HnClient {
    fn new(base: impl Into<String>, hn_base: impl Into<String>) -> Self {
        HnClient { base: base.into(), hn_base: hn_base.into() }
    }
}

impl HnApi for HnClient {
    fn user(&self, username: &str) -> Result<Option<Value>> {
        let url = format!("{}/user/{}.json", self.base, username);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let v: Value = resp.into_json().context("parsing user profile")?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(v))
    }

    fn item(&self, id: u64) -> Result<Option<Value>> {
        let url = format!("{}/item/{}.json", self.base, id);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let v: Value = resp.into_json().with_context(|| format!("parsing item {id}"))?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(v))
    }

    fn favorites_page(&self, username: &str, page: u32) -> Result<String> {
        let url = format!("{}/favorites?id={}&p={}", self.hn_base, username, page);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("User-Agent", "Trove/1 (personal data vault; personal use only)")
            .call()
            .with_context(|| format!("GET {url}"))?;
        resp.into_string().context("reading favorites HTML")
    }
}

/// Fetch-user-profile helper exposed for `def_connect` validation (real HTTP).
fn fetch_user_profile(base: &str, username: &str) -> Result<Option<Value>> {
    HnClient::new(base, HN_BASE).user(username)
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// One resolved HN item → a `social` contract [`Post`].
///
/// - Stories (`type == "story"`) → `kind:"post"`, `title`, `url`, `score` in extra.
/// - Comments (`type == "comment"`) → `kind:"comment"`, `text` as body,
///   `parent` id in `reply_to` + extra.
/// - Other types (job, poll, pollopt) → `kind` set to the HN type, best-effort
///   fields filled.
///
/// Returns `None` if the item has no `time` (the partition key) or `id`.
pub(crate) fn item_to_post(item: &Value) -> Option<Post> {
    let obj = item.as_object()?;
    let id = obj.get("id").and_then(Value::as_u64)?;
    let unix = obj.get("time").and_then(Value::as_i64)?;
    let ts = DateTime::from_timestamp(unix, 0)?.with_timezone(&Local).to_rfc3339();

    let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("story");
    let kind = match item_type {
        "comment" => "comment",
        "story" => "post",
        other => other, // job, poll, pollopt
    };

    let title = obj.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let url = obj.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let text = obj.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    let parent = obj.get("parent").and_then(Value::as_u64);

    let mut post = Post::new(SOURCE, id.to_string(), ts);
    post.kind = kind.to_string();
    if !title.is_empty() {
        post.title = title;
    }
    if !url.is_empty() {
        post.url = url;
    }
    if !text.is_empty() {
        post.text = text;
    }
    if let Some(p) = parent {
        post.reply_to = p.to_string();
        // For comments the thread root is the story; parent is the immediate
        // parent (may be another comment). We don't resolve the thread here —
        // keeping it to what the API returns directly.
    }

    // Source-specific fields → extra (score, descendants, parent, kids count).
    let mut extra = Map::new();
    if let Some(s) = obj.get("score").and_then(Value::as_i64) {
        extra.insert("score".into(), json!(s));
    }
    if let Some(d) = obj.get("descendants").and_then(Value::as_i64) {
        extra.insert("descendants".into(), json!(d));
    }
    if let Some(p) = obj.get("parent").and_then(Value::as_u64) {
        extra.insert("parent".into(), json!(p));
    }
    // Deleted / dead items — keep them (full fidelity), flag in extra.
    if obj.get("deleted").and_then(Value::as_bool) == Some(true) {
        extra.insert("deleted".into(), json!(true));
    }
    if obj.get("dead").and_then(Value::as_bool) == Some(true) {
        extra.insert("dead".into(), json!(true));
    }
    post.extra = extra;

    Some(post)
}

// ---------------------------------------------------------------------------
// Raw line type — carries ts for partitioning, serializes as the item verbatim.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Favorites scraper — isolated, gracefully degrading.

/// One favorite item as stored in `social/hacker-news/favorites.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FavoriteRef {
    /// HN item id.
    id: u64,
    /// RFC3339 time this entry was collected.
    ts_collected: String,
    /// Best-effort title from the page, if parseable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    title: String,
    /// Best-effort URL from the page.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    url: String,
}

/// Scrape the HN favorites page and return item refs. Returns an empty vec and
/// a human-readable note on any failure (never propagates an error — the caller
/// decides whether to surface it).
fn scrape_favorites(client: &impl HnApi, username: &str) -> (Vec<FavoriteRef>, Option<String>) {
    let ts_collected = Local::now().to_rfc3339();
    let mut refs = Vec::new();
    let mut page = 1u32;

    loop {
        let html = match client.favorites_page(username, page) {
            Ok(h) => h,
            Err(e) => return (refs, Some(format!("favorites fetch failed (page {page}): {e}"))),
        };

        // Private profile or no favorites: HN returns a page with no `athing`
        // rows and no `morelink`.
        let items_on_page = parse_favorites_html(&html, &ts_collected);
        let has_more = html_has_more(&html);

        refs.extend(items_on_page);

        if !has_more {
            break;
        }
        page += 1;
        // Polite delay between scrape pages.
        std::thread::sleep(Duration::from_millis(800));

        // Safety cap: HN favorites pages are numbered; cap at 100 pages
        // (5 000 items) to avoid an infinite loop on a malformed `morelink`.
        if page > 100 {
            break;
        }
    }

    let note = if refs.is_empty() && page == 1 {
        Some(format!(
            "no favorites found for {username:?} — profile may be private or favorites list is empty"
        ))
    } else {
        None
    };
    (refs, note)
}

/// Parse one HN favorites/newlinks HTML page: extract item id, title, url from
/// `tr.athing` rows.
///
/// HN HTML (current shape, confirmed 2024+):
/// ```html
/// <tr class="athing" id="12345678">
///   <td class="title"><span class="titleline"><a href="https://example.com">Title text</a>…</span></td>
/// </tr>
/// ```
/// Older shape had `<a class="storylink" …>` directly; we handle both.
///
/// Parsing strategy: scan the raw HTML for each occurrence of `"athing"`,
/// extract the id from the surrounding `<tr>` tag, then extract title/url
/// from the next ~600 bytes. This is layout-agnostic (works on single-line
/// or multi-line HTML).
pub(crate) fn parse_favorites_html(html: &str, ts_collected: &str) -> Vec<FavoriteRef> {
    let mut refs = Vec::new();
    let mut search_from = 0;

    while let Some(pos) = html[search_from..].find("athing") {
        let abs_pos = search_from + pos;
        // Back up to find the enclosing `<tr` so we can read the `id` attr.
        // Look back up to 200 bytes for `<tr`.
        let tr_search_start = abs_pos.saturating_sub(200);
        let Some(tr_rel) = html[tr_search_start..abs_pos].rfind("<tr") else {
            search_from = abs_pos + 1;
            continue;
        };
        let tr_pos = tr_search_start + tr_rel;
        // Find the end of this `<tr` tag.
        let tag_end = html[tr_pos..].find('>').unwrap_or(0);
        let tr_tag = &html[tr_pos..tr_pos + tag_end + 1];

        let Some(id) = extract_athing_id(tr_tag) else {
            search_from = abs_pos + 1;
            continue;
        };

        // Extract title/url from the next ~600 bytes after the tag.
        let after_tag = tr_pos + tag_end + 1;
        let snippet_end = html.len().min(after_tag + 600);
        let snippet = &html[after_tag..snippet_end];
        let (title, url) = extract_title_url(&[snippet]);

        refs.push(FavoriteRef {
            id,
            ts_collected: ts_collected.to_string(),
            title,
            url,
        });

        // Continue searching after this `athing` marker.
        search_from = abs_pos + 7;
    }
    refs
}

/// Return the numeric id from an `athing` row, or `None`.
fn extract_athing_id(line: &str) -> Option<u64> {
    // Match either order of `class`/`id` attributes.
    if !line.contains("athing") {
        return None;
    }
    // Look for `id="<digits>"` somewhere on the line.
    let id_attr = "id=\"";
    let start = line.find(id_attr)?;
    let after = &line[start + id_attr.len()..];
    let end = after.find('"')?;
    let id_str = &after[..end];
    id_str.parse::<u64>().ok()
}

/// Extract the story title and URL from a snippet of HTML.
/// Handles both `<span class="titleline"><a href="…">Title</a>` (current) and
/// `<a class="storylink" href="…">Title</a>` (older).
fn extract_title_url(snippets: &[&str]) -> (String, String) {
    let joined = snippets.join(" ");
    // Current shape: `<span class="titleline"><a href="...">Title</a>`.
    if let Some((title, url)) = extract_span_titleline(&joined) {
        return (title, url);
    }
    // Older shape: `<a class="storylink" href="...">Title</a>`.
    if let Some((title, url)) = extract_storylink(&joined) {
        return (title, url);
    }
    (String::new(), String::new())
}

fn extract_span_titleline(html: &str) -> Option<(String, String)> {
    // Find `class="titleline"` then the first `<a href=` after it.
    let marker = "class=\"titleline\"";
    let after_span = html.find(marker)?;
    let rest = &html[after_span..];
    extract_first_a_href_and_text(rest)
}

fn extract_storylink(html: &str) -> Option<(String, String)> {
    // Find `class="storylink"` then go back to find its `href`.
    let marker = "class=\"storylink\"";
    let pos = html.find(marker)?;
    // Search leftward from `pos` to find `<a` to extract href and text.
    let snippet = &html[pos.saturating_sub(200)..html.len().min(pos + 500)];
    extract_first_a_href_and_text(snippet)
}

/// Find the first `<a href="...">...</a>` in `html`, returning (inner text, href).
fn extract_first_a_href_and_text(html: &str) -> Option<(String, String)> {
    let href_marker = "href=\"";
    let a_start = html.find("<a ")?;
    let rest = &html[a_start..];
    let href_pos = rest.find(href_marker)?;
    let href_rest = &rest[href_pos + href_marker.len()..];
    let href_end = href_rest.find('"')?;
    let url = decode_html_entities(&href_rest[..href_end]);

    // Extract inner text: from `>` to `</a>`.
    let inner_start = rest.find('>')?;
    let inner = &rest[inner_start + 1..];
    let inner_end = inner.find("</a>")?;
    let title = strip_html_tags(&inner[..inner_end]).trim().to_string();

    Some((title, url))
}

/// Detect a pagination "more" link — HN uses `<a class="morelink"` (or single-quoted
/// `class='morelink'`) for next page.  Live HN pages use single quotes; accept both.
fn html_has_more(html: &str) -> bool {
    html.contains("class=\"morelink\"") || html.contains("class='morelink'")
}

/// Minimal HTML entity decode for the common entities in HN URLs/titles.
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
}

/// Strip any `<tag>` from text so titles don't carry embedded HTML.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_html_entities(&out)
}

// ---------------------------------------------------------------------------
// Pull — resolve submitted ids, write contract + raw, then scrape favorites.

/// Load existing guids from the contract stream for dedupe.
fn load_seen_guids(vault: &Vault) -> Result<HashSet<String>> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut seen = HashSet::new();
    for key in stream.partitions()? {
        for p in stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen.insert(p.guid);
            }
        }
    }
    Ok(seen)
}

/// Load existing favorite ids for dedupe.
fn load_seen_fav_ids(vault: &Vault) -> HashSet<u64> {
    let mut seen = HashSet::new();
    let Ok(path) = vault.resolve(FAV_FILE) else { return seen };
    let Ok(body) = std::fs::read_to_string(&path) else { return seen };
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(f) = serde_json::from_str::<FavoriteRef>(line) {
            seen.insert(f.id);
        }
    }
    seen
}

/// Append new favorites to `social/hacker-news/favorites.jsonl`.
fn append_favorites(vault: &Vault, refs: &[FavoriteRef]) -> Result<()> {
    use std::io::Write;
    if refs.is_empty() {
        return Ok(());
    }
    let path = vault.resolve(FAV_FILE)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {FAV_FILE}"))?;
    for r in refs {
        writeln!(f, "{}", serde_json::to_string(r)?)?;
    }
    Ok(())
}

/// Core pull — injectable API client for tests.
pub(crate) fn pull_with(vault: &Vault, client: &impl HnApi, username: &str) -> Result<PullOutcome> {
    let mut state = vault.read_hn_sync();
    let mut seen = load_seen_guids(vault)?;

    // Step 1: fetch the user profile to get the `submitted[]` id list.
    let profile = client.user(username)?.context("user not found on HN")?;
    let submitted: Vec<u64> = profile
        .get("submitted")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();

    // Filter to only ids strictly greater than our watermark (incremental).
    // On a first run, the watermark is None → fetch all.
    let to_fetch: Vec<u64> = submitted
        .into_iter()
        .filter(|&id| state.max_id.map_or(true, |w| id > w))
        .collect();

    let contract_stream = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let mut new_posts: Vec<Post> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    let mut max_id_this_run: Option<u64> = None;

    for id in &to_fetch {
        let Some(item) = client.item(*id)? else {
            // Item deleted or never existed — skip; don't advance watermark past it.
            continue;
        };

        // Track max id encountered (even items we skip due to parse/dedupe failure).
        let item_id = item.get("id").and_then(Value::as_u64).unwrap_or(*id);
        max_id_this_run = Some(max_id_this_run.map_or(item_id, |m: u64| m.max(item_id)));

        let Some(post) = item_to_post(&item) else { continue };
        if !seen.insert(post.guid.clone()) {
            continue; // already stored
        }
        let ts = post.ts.clone();
        new_raws.push(RawLine { ts, value: item });
        new_posts.push(post);

        // Polite delay between item fetches.
        if to_fetch.len() > 1 {
            std::thread::sleep(REQ_INTERVAL);
        }
    }

    let items_written = new_posts.len() as u64;
    contract_stream.append(&new_posts, |p| &p.ts)?;
    raw_stream.append(&new_raws, |r| &r.ts)?;

    // Advance the watermark only if we made forward progress.
    if let Some(m) = max_id_this_run {
        if state.max_id.map_or(true, |w| m > w) {
            state.max_id = Some(m);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_hn_sync(&state)?;

    // Step 2: scrape favorites (gracefully degraded — never fails the pull).
    let seen_favs = load_seen_fav_ids(vault);
    let (mut new_favs, fav_note) = scrape_favorites(client, username);
    new_favs.retain(|f| !seen_favs.contains(&f.id));
    let favs_written = new_favs.len() as u64;
    append_favorites(vault, &new_favs)?;

    let mut headline = format!("{items_written} new items synced");
    if favs_written > 0 {
        headline.push_str(&format!(", {favs_written} favorites"));
    }
    if let Some(note) = fav_note {
        headline.push_str(&format!(" (favorites: {note})"));
    }

    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("items", items_written),
            ("favorites", favs_written),
        ]),
    })
}

/// Public pull entry point (resolves credentials, calls [`pull_with`]).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let username = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|u| !u.trim().is_empty())
        .context("Hacker News is not connected — add your username in the Integrations tab")?;
    let client = HnClient::new(FIREBASE_BASE, HN_BASE);
    pull_with(vault, &client, &username)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-hn-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Stub API client for offline tests.

    struct StubApi {
        user_profile: Value,
        items: std::collections::HashMap<u64, Value>,
        favorites_html: Arc<Mutex<Vec<String>>>,
    }

    impl StubApi {
        fn new(profile: Value, items: impl IntoIterator<Item = (u64, Value)>) -> Self {
            StubApi {
                user_profile: profile,
                items: items.into_iter().collect(),
                favorites_html: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_favorites(mut self, pages: Vec<String>) -> Self {
            self.favorites_html = Arc::new(Mutex::new(pages));
            self
        }
    }

    impl HnApi for StubApi {
        fn user(&self, _username: &str) -> Result<Option<Value>> {
            if self.user_profile.is_null() {
                return Ok(None);
            }
            Ok(Some(self.user_profile.clone()))
        }
        fn item(&self, id: u64) -> Result<Option<Value>> {
            Ok(self.items.get(&id).cloned())
        }
        fn favorites_page(&self, _username: &str, page: u32) -> Result<String> {
            let pages = self.favorites_html.lock().unwrap();
            let idx = (page - 1) as usize;
            Ok(pages.get(idx).cloned().unwrap_or_default())
        }
    }

    fn make_story(id: u64, time: i64, title: &str, url: &str, score: i64) -> Value {
        json!({
            "by": "testuser",
            "descendants": 5,
            "id": id,
            "kids": [],
            "score": score,
            "time": time,
            "title": title,
            "type": "story",
            "url": url
        })
    }

    fn make_comment(id: u64, time: i64, text: &str, parent: u64) -> Value {
        json!({
            "by": "testuser",
            "id": id,
            "kids": [],
            "parent": parent,
            "text": text,
            "time": time,
            "type": "comment"
        })
    }

    fn make_profile(username: &str, submitted: &[u64]) -> Value {
        json!({
            "id": username,
            "created": 1400000000i64,
            "karma": 100,
            "submitted": submitted
        })
    }

    // -----------------------------------------------------------------------
    // item_to_post

    #[test]
    fn story_to_post_maps_fields() {
        let item = make_story(8863, 1175714200, "My YC app: Dropbox", "https://example.com", 104);
        let post = item_to_post(&item).unwrap();
        assert_eq!(post.guid, "8863");
        assert_eq!(post.source, "hacker-news");
        assert_eq!(post.kind, "post");
        assert_eq!(post.title, "My YC app: Dropbox");
        assert_eq!(post.url, "https://example.com");
        assert!(post.text.is_empty(), "stories have no body text");
        assert_eq!(post.extra.get("score"), Some(&json!(104)));
        assert_eq!(post.extra.get("descendants"), Some(&json!(5)));
    }

    #[test]
    fn comment_to_post_maps_fields() {
        let item = make_comment(2921983, 1314211127, "Aw shucks, guys!", 2921506);
        let post = item_to_post(&item).unwrap();
        assert_eq!(post.guid, "2921983");
        assert_eq!(post.kind, "comment");
        assert_eq!(post.text, "Aw shucks, guys!");
        assert_eq!(post.reply_to, "2921506");
        assert!(post.title.is_empty());
        assert!(post.url.is_empty());
        assert_eq!(post.extra.get("parent"), Some(&json!(2921506u64)));
    }

    #[test]
    fn deleted_item_gets_deleted_flag_in_extra() {
        let item = json!({
            "id": 9999u64,
            "time": 1314211127i64,
            "type": "story",
            "deleted": true
        });
        let post = item_to_post(&item).unwrap();
        assert_eq!(post.extra.get("deleted"), Some(&json!(true)));
    }

    #[test]
    fn item_without_time_returns_none() {
        let item = json!({"id": 1u64, "type": "story", "title": "no time"});
        assert!(item_to_post(&item).is_none(), "must have time to partition");
    }

    // -----------------------------------------------------------------------
    // Favorites HTML parser.

    fn fav_html_page(items: &[(u64, &str, &str)], has_more: bool) -> String {
        let mut rows = String::new();
        for (id, title, url) in items {
            rows.push_str(&format!(
                r#"<tr class="athing" id="{id}"><td class="title"><span class="titleline"><a href="{url}">{title}</a></span></td></tr>"#
            ));
        }
        // Use single-quoted attributes to match the form live news.ycombinator.com serves,
        // so the parser is tested against realistic HTML (not a self-consistent double-quoted
        // fixture that only the old broken detector could match).
        let more = if has_more {
            "<a href='/favorites?id=user&p=2' class='morelink' rel='next'>More</a>"
        } else {
            ""
        };
        format!("<html><body><table>{rows}</table>{more}</body></html>")
    }

    #[test]
    fn parse_favorites_extracts_current_html_shape() {
        let html = fav_html_page(
            &[
                (41250912, "Show HN: A local-first vault", "https://example.com/vault"),
                (39123456, "Ask HN: Favorite tools 2024", "https://news.ycombinator.com/item?id=39123456"),
            ],
            false,
        );
        let refs = parse_favorites_html(&html, "2026-06-17T00:00:00-07:00");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].id, 41250912);
        assert_eq!(refs[0].title, "Show HN: A local-first vault");
        assert_eq!(refs[0].url, "https://example.com/vault");
        assert_eq!(refs[1].id, 39123456);
    }

    #[test]
    fn parse_favorites_tolerates_older_storylink_shape() {
        let html = r#"<html><body><table>
          <tr class="athing" id="12345678">
            <td><a class="storylink" href="https://old.example.com">Old title</a></td>
          </tr>
        </table></body></html>"#;
        let refs = parse_favorites_html(html, "2026-06-17T00:00:00-07:00");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, 12345678);
        assert_eq!(refs[0].title, "Old title");
        assert_eq!(refs[0].url, "https://old.example.com");
    }

    #[test]
    fn private_profile_yields_empty_refs_and_note() {
        let html = "<html><body><p>No favorites to show.</p></body></html>";
        let refs = parse_favorites_html(html, "2026-06-17T00:00:00-07:00");
        assert!(refs.is_empty(), "no athing rows → empty");
    }

    #[test]
    fn html_has_more_detects_morelink() {
        // Double-quoted class (original fixture form).
        assert!(html_has_more(r#"<a class="morelink" href="?p=2">More</a>"#));
        // Single-quoted class — this is the form live news.ycombinator.com actually serves.
        assert!(html_has_more("<a href='?p=2' class='morelink' rel='next'>More</a>"));
        assert!(!html_has_more("<html><body><p>end</p></body></html>"));
    }

    #[test]
    fn html_entities_decoded_in_urls_and_titles() {
        let html = r#"<html><body><table>
          <tr class="athing" id="99999">
            <td class="title"><span class="titleline"><a href="https://example.com?a=1&amp;b=2">A &amp; B</a></span></td>
          </tr>
        </table></body></html>"#;
        let refs = parse_favorites_html(html, "2026-06-17T00:00:00-07:00");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://example.com?a=1&b=2");
        assert_eq!(refs[0].title, "A & B");
    }

    // -----------------------------------------------------------------------
    // pull_with — integration-level tests (offline).

    #[test]
    fn first_sync_fetches_all_submitted_items() {
        let v = temp_vault("first_sync");
        let story = make_story(8863, 1175714200, "Dropbox", "https://example.com", 104);
        let comment = make_comment(9000, 1175800000, "nice!", 8863);
        let profile = make_profile("testuser", &[8863, 9000]);
        let client = StubApi::new(profile, [(8863, story), (9000, comment)]);

        let out = pull_with(&v, &client, "testuser").unwrap();
        assert_eq!(out.counts.get("items"), Some(&2));

        // Contract rows partitioned by ts month.
        let stream = v.stream(DIR, Partition::Month);
        let partitions = stream.partitions().unwrap();
        assert!(!partitions.is_empty(), "at least one month written");
        let all_posts: Vec<Post> = partitions
            .iter()
            .flat_map(|k| stream.read::<Post>(k).unwrap())
            .collect();
        assert_eq!(all_posts.len(), 2);

        // guid = item id
        let story_post = all_posts.iter().find(|p| p.kind == "post").unwrap();
        assert_eq!(story_post.guid, "8863");
        assert_eq!(story_post.title, "Dropbox");

        let comment_post = all_posts.iter().find(|p| p.kind == "comment").unwrap();
        assert_eq!(comment_post.reply_to, "8863");
    }

    #[test]
    fn incremental_sync_only_fetches_new_ids() {
        let v = temp_vault("incremental");
        let story = make_story(8863, 1175714200, "Dropbox", "https://x.com", 104);
        let profile = make_profile("testuser", &[8863]);
        let client = StubApi::new(profile, [(8863, story)]);
        pull_with(&v, &client, "testuser").unwrap();

        // Second sync: same profile + a new story id 9999.
        let new_story = make_story(9999, 1176000000, "New thing", "https://new.example.com", 50);
        let profile2 = make_profile("testuser", &[9999, 8863]);
        let client2 = StubApi::new(profile2, [(9999, new_story)]);
        let out2 = pull_with(&v, &client2, "testuser").unwrap();
        // Only the NEW id (9999) should be fetched; 8863 is below the watermark.
        assert_eq!(out2.counts.get("items"), Some(&1));

        // Total posts in vault = 2.
        let stream = v.stream(DIR, Partition::Month);
        let total: usize = stream
            .partitions()
            .unwrap()
            .iter()
            .map(|k| stream.read::<Post>(k).unwrap().len())
            .sum();
        assert_eq!(total, 2, "both items in vault, no duplicate");
    }

    #[test]
    fn reimport_dedupes_existing_guids() {
        let v = temp_vault("dedupe");
        let story = make_story(8863, 1175714200, "Dropbox", "https://example.com", 104);
        let profile = make_profile("testuser", &[8863]);
        let client = StubApi::new(profile.clone(), [(8863, story.clone())]);
        let out1 = pull_with(&v, &client, "testuser").unwrap();
        assert_eq!(out1.counts.get("items"), Some(&1));

        // Simulate a sync where the watermark already covers the id.
        let client2 = StubApi::new(profile, [(8863, story)]);
        let out2 = pull_with(&v, &client2, "testuser").unwrap();
        assert_eq!(out2.counts.get("items"), Some(&0), "dedupe: no re-write");
    }

    #[test]
    fn raw_layer_written_alongside_contract() {
        let v = temp_vault("raw_layer");
        let story = make_story(8863, 1175714200, "Dropbox", "https://x.com", 104);
        let profile = make_profile("testuser", &[8863]);
        let client = StubApi::new(profile, [(8863, story)]);
        pull_with(&v, &client, "testuser").unwrap();

        // Raw file: social/hacker-news/raw/YYYY-MM.jsonl.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let raw_partitions = raw_stream.partitions().unwrap();
        assert!(!raw_partitions.is_empty(), "raw layer written");
        let raw_lines: Vec<Value> = raw_partitions
            .iter()
            .flat_map(|k| raw_stream.read::<Value>(k).unwrap())
            .collect();
        assert!(!raw_lines.is_empty());
        // The raw line is the item verbatim.
        let raw = &raw_lines[0];
        assert_eq!(raw.get("id"), Some(&json!(8863u64)));
        assert_eq!(raw.get("type"), Some(&json!("story")));
        assert_eq!(raw.get("score"), Some(&json!(104i64)));
    }

    #[test]
    fn favorites_written_to_flat_file() {
        let v = temp_vault("favorites");
        let profile = make_profile("testuser", &[]);
        let fav_page = fav_html_page(
            &[(41250912, "Show HN: Vault", "https://vault.example.com")],
            false,
        );
        let client = StubApi::new(profile, []).with_favorites(vec![fav_page]);
        pull_with(&v, &client, "testuser").unwrap();

        let path = v.root().join("social/hacker-news/favorites.jsonl");
        assert!(path.exists(), "favorites.jsonl written");
        let body = std::fs::read_to_string(&path).unwrap();
        let fav: FavoriteRef = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(fav.id, 41250912);
        assert_eq!(fav.title, "Show HN: Vault");
    }

    #[test]
    fn favorites_dedupe_across_runs() {
        let v = temp_vault("fav_dedupe");
        let profile = make_profile("testuser", &[]);
        let fav_page = fav_html_page(
            &[(41250912, "Show HN: Vault", "https://vault.example.com")],
            false,
        );
        let client = StubApi::new(profile.clone(), []).with_favorites(vec![fav_page.clone()]);
        pull_with(&v, &client, "testuser").unwrap();

        // Second run — same favorites page.
        let client2 = StubApi::new(profile, []).with_favorites(vec![fav_page]);
        let out2 = pull_with(&v, &client2, "testuser").unwrap();
        assert_eq!(out2.counts.get("favorites"), Some(&0), "favorite already stored, no dupe");

        let path = v.root().join("social/hacker-news/favorites.jsonl");
        // Just check that the file has exactly 1 line (not 2).
        let body = std::fs::read_to_string(&path).unwrap();
        let count = body.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(count, 1, "exactly one favorite line, not duplicated");
    }

    #[test]
    fn missing_user_returns_error() {
        let v = temp_vault("missing_user");
        let client = StubApi::new(Value::Null, []);
        let err = pull_with(&v, &client, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("user not found"), "{err}");
    }

    #[test]
    fn old_post_lines_still_deserialize() {
        // Back-compat: a minimal sparse social line still parses as Post.
        let line = r#"{"ts":"2024-06-10T00:00:00-07:00","source":"hacker-news","guid":"8863"}"#;
        let p: Post = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "8863");
        assert_eq!(p.kind, "");
        assert!(p.text.is_empty());
    }

    #[test]
    fn sync_state_persisted_and_read_back() {
        let v = temp_vault("sync_state");
        let state = SyncState { max_id: Some(99999), updated: Some("2026-06-17T00:00:00+00:00".into()) };
        v.write_hn_sync(&state).unwrap();
        let back = v.read_hn_sync();
        assert_eq!(back.max_id, Some(99999));
    }
}
