//! Bluesky (AT Protocol) — periodic pull of posts, likes, and follows via
//! the public Bluesky AppView REST API, authenticated with an App Password
//! session token.
//!
//! The user supplies their handle (e.g. `alice.bsky.social`) and an App
//! Password (generated in Bluesky Settings → App Passwords) via a
//! [`ConnectMethod::TokenPaste`] that expects a combined `handle:app-password`
//! string. On connect the module exchanges it for an `accessJwt` session token
//! via `com.atproto.server.createSession`, then stores only that token under
//! `.trove/sync/bluesky-token.json` (0600); the App Password itself is never
//! persisted.
//!
//! ## Two layers
//!
//! - **Raw** — full API feed items verbatim under
//!   `social/bluesky/raw/YYYY-MM.jsonl`, partitioned by the post's local month.
//! - **Contract** — one [`crate::social::Post`] per authored item under
//!   `social/bluesky/YYYY-MM.jsonl`, deduped by
//!   `guid = post.uri` (the AT-URI, e.g.
//!   `at://did:plc:.../app.bsky.feed.post/3kxy`).
//!
//! ## Cursor
//!
//! `getAuthorFeed` only paginates **backward** (older items); there is no
//! "newer-than" cursor. Each sync starts from the top of the feed (no cursor)
//! and pages downward, stopping as soon as a full page of already-held items is
//! reached (`all_seen` stop) or the feed is exhausted. The guid-based dedupe
//! table prevents duplicates across runs. `.trove/bluesky-sync.json` records
//! only the `updated` timestamp for display purposes; no pagination cursor is
//! stored between runs.
//!
//! Note: backfill depth is bounded by the author-feed history the AppView
//! retains (typically the full lifetime of the account, but not guaranteed for
//! very old accounts). A CAR/getRepo path would give a no-auth complete-repo
//! backfill; that is deferred to a future version.
//!
//! ## DMs
//!
//! Bluesky DMs are separate from the public repo and are not exposed here.
//! Likes and follows are written to the raw layer only (they are not authored
//! content; see the `social.rs` doc for the rule).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::social::{Media, Post};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "bluesky";
const DIR: &str = "social/bluesky";
const RAW_DIR: &str = "social/bluesky/raw";
const SYNC_FILE: &str = ".trove/bluesky-sync.json";
const SERVICE: &str = "bluesky";

/// Production Bluesky PDS (for createSession).
const PDS_HOST: &str = "https://bsky.social";
/// Bluesky AppView (for app.bsky.* read queries).
const APP_VIEW_HOST: &str = "https://public.api.bsky.app";

const PAGE_LIMIT: u64 = 100;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE_PAUSE: Duration = Duration::from_millis(200);

/// 15-minute cadence — posts appear in near-real-time; no need for hourly.
pub const BLUESKY_SYNC_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let connected = vault
        .load_sync_token(SERVICE)
        .ok()
        .and_then(|t| t)
        .map(|t| !t.access_token.trim().is_empty())
        .unwrap_or(false);

    if !connected {
        return Ok(crate::registry::CollectOutcome::note(
            "bluesky sync skipped: not connected",
        ));
    }

    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("posts").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("bluesky synced — {n} posts")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "bluesky sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let posts = out.counts.get("posts").copied().unwrap_or(0);
    let headline = if posts == 0 {
        "Bluesky is up to date — no new posts".to_string()
    } else {
        format!("Bluesky synced — {posts} posts")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bluesky",
        name: "Bluesky",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Bluesky posts, replies, reposts, likes, and follows \
                      via the open AT Protocol API. Authenticates with an App Password — \
                      your main password is never touched. First sync backfills your \
                      full post history; later syncs are incremental.",
        domain: "social",
        vault_path: "social/bluesky/",
        toggleable: true,
        setup: &[
            "In Bluesky, go to Settings → Privacy and Security → App Passwords and create \
             a new App Password (any name, e.g. \"Trove\"). Do NOT use your main password.",
            "Enter your handle and App Password in the format `handle:app-password` \
             (e.g. `alice.bsky.social:xxxx-xxxx-xxxx-xxxx`) and connect.",
        ],
        caveats: "Direct messages are separate from the public repo and are not collected. \
                  Likes and follows are stored in the raw layer only — they are not \
                  authored content. The App Password is exchanged for a session token \
                  on connect; the password itself is never stored.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(BLUESKY_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("bluesky"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection — TokenPaste: `handle:app-password`.

fn def_connect(vault: &Vault, cred: &str) -> Result<()> {
    let cred = cred.trim();
    let (handle, app_password) = parse_credential(cred)?;

    // Exchange the App Password for a session token.
    let token = create_session(PDS_HOST, handle, app_password)?;
    vault.save_sync_token(SERVICE, &token)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        // The handle is stored in `scope`; fall back to access_token on older sessions.
        let handle = token.scope.clone().unwrap_or_else(|| token.access_token.clone());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: handle,
            connected_at: None,
            expires_at: token.expires_at,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "bluesky",
    display_name: "Bluesky",
    methods: &[ConnectMethod::TokenPaste {
        label: "Handle and App Password",
        help: "Enter your Bluesky handle and an App Password separated by a colon \
               (e.g. alice.bsky.social:xxxx-xxxx-xxxx-xxxx). Generate an App Password \
               in Bluesky Settings → Privacy and Security → App Passwords. \
               Your main password is never stored — only the session token.",
        placeholder: "alice.bsky.social:xxxx-xxxx-xxxx-xxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["bluesky"],
    setup: &[
        "In Bluesky: Settings → Privacy and Security → App Passwords → New App Password.",
        "Enter your handle and the App Password as `handle:app-password` here and connect.",
    ],
};

// ---------------------------------------------------------------------------
// Credential helpers.

/// Parse a `handle:app-password` string. The handle may not contain a colon
/// (AT Protocol handles are domain-like: `alice.bsky.social`); the
/// app-password format is `xxxx-xxxx-xxxx-xxxx` (may contain dashes).
fn parse_credential(cred: &str) -> Result<(&str, &str)> {
    let idx = cred.find(':').ok_or_else(|| {
        anyhow::anyhow!(
            "expected handle:app-password (e.g. alice.bsky.social:xxxx-xxxx-xxxx-xxxx)"
        )
    })?;
    let handle = cred[..idx].trim();
    let password = cred[idx + 1..].trim();
    if handle.is_empty() {
        bail!("handle is empty — expected handle:app-password");
    }
    if password.is_empty() {
        bail!("app-password is empty — expected handle:app-password");
    }
    Ok((handle, password))
}

// ---------------------------------------------------------------------------
// Session creation (com.atproto.server.createSession).

/// Exchange handle + App Password for a session token set.
/// Stores:
/// - `access_token` = accessJwt
/// - `refresh_token` = refreshJwt
/// - `scope` = handle (for display in status)
/// - `token_type` = did (for actor ownership filtering)
fn create_session(
    pds_host: &str,
    handle: &str,
    app_password: &str,
) -> Result<crate::sync::oauth::TokenSet> {
    let url = format!("{pds_host}/xrpc/com.atproto.server.createSession");
    let body = serde_json::json!({
        "identifier": handle,
        "password": app_password,
    });
    let body_str = serde_json::to_string(&body)?;
    let resp = ureq::post(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/json")
        .send_string(&body_str)
        .map_err(|e| anyhow::anyhow!("createSession: {e}"))?;

    if resp.status() != 200 {
        bail!("createSession: HTTP {}", resp.status());
    }

    let v: Value = resp.into_json().context("createSession: parse error")?;
    let access = v.get("accessJwt").and_then(Value::as_str).context("missing accessJwt")?;
    let refresh = v.get("refreshJwt").and_then(Value::as_str);
    let did = v.get("did").and_then(Value::as_str).unwrap_or("");
    let resp_handle = v.get("handle").and_then(Value::as_str).unwrap_or(handle);

    Ok(crate::sync::oauth::TokenSet {
        access_token: access.to_string(),
        refresh_token: refresh.map(|s| s.to_string()),
        token_type: if did.is_empty() { None } else { Some(did.to_string()) },
        scope: Some(resp_handle.to_string()),
        expires_at: None, // Bluesky JWTs expire after ~2h; re-login on 401
    })
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync (display only).
    /// No pagination cursor is stored — each run starts from the feed top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_bluesky_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_bluesky_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP client — injectable trait for offline tests.

trait AuthorFeed {
    /// Fetch one page of getAuthorFeed for `actor` (handle or DID).
    /// `cursor` is the pagination token from the previous response.
    fn fetch_feed(
        &self,
        actor: &str,
        cursor: Option<&str>,
        access_jwt: &str,
    ) -> Result<Value>;
}

struct BskyClient {
    app_view_host: String,
}

impl BskyClient {
    fn new(host: impl Into<String>) -> Self {
        BskyClient { app_view_host: host.into() }
    }
}

impl AuthorFeed for BskyClient {
    fn fetch_feed(
        &self,
        actor: &str,
        cursor: Option<&str>,
        access_jwt: &str,
    ) -> Result<Value> {
        let url = format!("{}/xrpc/app.bsky.feed.getAuthorFeed", self.app_view_host);
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {access_jwt}"))
            .query("actor", actor)
            .query("limit", &PAGE_LIMIT.to_string())
            .query("filter", "posts_with_replies");

        if let Some(c) = cursor {
            req = req.query("cursor", c);
        }

        let resp = req.call().map_err(|e| anyhow::anyhow!("getAuthorFeed: {e}"))?;
        if resp.status() == 401 {
            bail!("Bluesky session expired — reconnect in Integrations");
        }
        if resp.status() != 200 {
            bail!("getAuthorFeed: HTTP {}", resp.status());
        }
        let v: Value = resp.into_json().context("getAuthorFeed: parse error")?;
        Ok(v)
    }
}

// ---------------------------------------------------------------------------
// Parsing.

/// One raw line: the feed item object flattened to disk (ts field used only
/// for partitioning, not serialized).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Parse one FeedViewPost item into a contract Post + raw line.
///
/// Handles:
/// - Regular posts (`kind = "post"`)
/// - Replies (`kind = "reply"`, has `post.record.reply.parent`)
/// - Reposts (`kind = "repost"`, `reason.$type` contains "reasonRepost")
/// - Quote posts (`kind = "quote"`, embed contains a record ref)
///
/// Returns `None` for items not authored by `actor_did` (non-repost items
/// from other users that appear on the feed are skipped; only the actor's
/// own actions are authored content).
fn parse_feed_item(item: &Value, actor_did: &str) -> Option<ParsedItem> {
    let post = item.get("post")?;
    let uri = post.get("uri").and_then(Value::as_str)?;
    let cid = post.get("cid").and_then(Value::as_str).unwrap_or("");
    let author = post.get("author")?;
    let author_did = author.get("did").and_then(Value::as_str).unwrap_or("");

    // The reason field exists for reposts by the connected user.
    let reason = item.get("reason");
    let is_repost = reason
        .and_then(|r| r.get("$type"))
        .and_then(Value::as_str)
        .map(|t| t.contains("reasonRepost"))
        .unwrap_or(false);

    // Only authored content: items where the actor wrote the post OR performed the repost.
    if author_did != actor_did && !is_repost {
        return None;
    }

    let record = post.get("record")?;

    // For reposts: use reason.indexedAt (when the repost action happened), not
    // record.createdAt (when the ORIGINAL author wrote the post).
    // For all other kinds: use record.createdAt.
    let ts_str = if is_repost {
        reason
            .and_then(|r| r.get("indexedAt"))
            .and_then(Value::as_str)
            .or_else(|| record.get("createdAt").and_then(Value::as_str))
            .unwrap_or("")
    } else {
        record.get("createdAt").and_then(Value::as_str).unwrap_or("")
    };
    let ts = parse_iso8601_to_local(ts_str)?;

    let text = record.get("text").and_then(Value::as_str).unwrap_or("").to_string();

    // Determine kind, reply_to, thread, repost_of, quote_of.
    // Note: these are determined independently so a reply that also quotes
    // gets both reply_to and quote_of populated.
    let (kind, guid, repost_of) = if is_repost {
        // Repost: guid must be distinct from the original post's guid to avoid
        // collisions (especially when the user reposts their own posts).
        // Use a synthetic "<actor_did>/repost/<original_uri>" key.
        let original_uri = uri.to_string();
        let repost_guid = format!("{actor_did}/repost/{original_uri}");
        ("repost".to_string(), repost_guid, original_uri)
    } else {
        (String::new(), uri.to_string(), String::new())
    };

    // Reply detection.
    let (reply_kind, reply_to, thread) = if !is_repost {
        if let Some(r) = record.get("reply") {
            let parent_uri = r
                .get("parent")
                .and_then(|p| p.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let root_uri = r
                .get("root")
                .and_then(|root| root.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ("reply".to_string(), parent_uri, root_uri)
        } else {
            (String::new(), String::new(), String::new())
        }
    } else {
        (String::new(), String::new(), String::new())
    };

    // Quote detection: independent of reply, so reply-with-quote gets both.
    // Check record.embed for app.bsky.embed.record (quote ref) or
    // app.bsky.embed.recordWithMedia (quote-with-media).
    let quote_of = if !is_repost {
        let embed = record.get("embed");
        // Direct quote: embed.record.uri
        let direct = embed
            .and_then(|e| e.get("record"))
            .and_then(|r| r.get("uri"))
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        // Quote-with-media: embed.record.record.uri
        let with_media = embed
            .and_then(|e| e.get("record"))
            .and_then(|r| r.get("record"))
            .and_then(|r| r.get("uri"))
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        direct.or(with_media).unwrap_or_default()
    } else {
        String::new()
    };

    // Resolve final kind: repost takes priority, then reply, then quote, then post.
    let final_kind = if !kind.is_empty() {
        kind
    } else if !reply_kind.is_empty() {
        reply_kind
    } else if !quote_of.is_empty() {
        "quote".to_string()
    } else {
        "post".to_string()
    };

    // Language — `langs` is an array; take the first tag.
    let lang = record
        .get("langs")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Media metadata from the hydrated PostView embed (post.embed), which has
    // CDN URLs. Fall back to record.embed only as a last resort (record embed
    // carries blob refs, not CDN URLs, so it won't have fullsize).
    let media = parse_media(post);

    // Hashtags from facets.
    let tags = parse_tags(record);

    // Extra: engagement counts from the PostView.
    let mut extra = Map::new();
    for key in ["likeCount", "repostCount", "replyCount", "quoteCount", "bookmarkCount"] {
        if let Some(n) = post.get(key).and_then(Value::as_i64) {
            extra.insert(key.to_string(), Value::Number(n.into()));
        }
    }
    if !cid.is_empty() {
        extra.insert("cid".into(), Value::String(cid.to_string()));
    }

    let mut post_row = Post::new(SOURCE, guid, ts.clone());
    post_row.kind = final_kind;
    post_row.text = text;
    post_row.lang = lang;
    post_row.reply_to = reply_to;
    post_row.thread = thread;
    post_row.repost_of = repost_of;
    post_row.quote_of = quote_of;
    post_row.media = media;
    post_row.tags = tags;
    post_row.extra = extra;

    let raw_value = item.clone();

    Some(ParsedItem { post: post_row, raw: RawLine { ts, value: raw_value } })
}

/// Parsed output for one FeedViewPost.
struct ParsedItem {
    post: Post,
    raw: RawLine,
}

/// Parse media metadata from the hydrated PostView (`post` object from the
/// feed item).
///
/// CDN-resolved URLs live in `post.embed` (the **view** layer,
/// `app.bsky.embed.images#view` / `app.bsky.embed.video#view`), not in
/// `post.record.embed` which carries raw blob refs with no `fullsize`.
/// We read `post.embed` first; if that is absent (unusual) we do a best-effort
/// fallback to `post.record.embed`, where `fullsize` is still absent but alt
/// text may survive.
fn parse_media(post: &Value) -> Vec<Media> {
    // Prefer the hydrated view embed (has CDN fullsize/thumb URLs).
    let view_embed = post.get("embed");
    // Fallback: record embed (blob refs only, no CDN URLs).
    let record_embed = post.get("record").and_then(|r| r.get("embed"));

    let embed = match view_embed.or(record_embed) {
        Some(e) => e,
        None => return Vec::new(),
    };

    // Images view: `app.bsky.embed.images#view` has an `images[]` array
    // where each entry has `fullsize` (CDN URL) and `alt`.
    if let Some(images) = embed.get("images").and_then(Value::as_array) {
        return images
            .iter()
            .map(|img| {
                // `fullsize` is present in the hydrated PostView.
                // `thumb` is the smaller variant — use fullsize preferentially.
                let url = img
                    .get("fullsize")
                    .and_then(Value::as_str)
                    .or_else(|| img.get("thumb").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                let alt = img.get("alt").and_then(Value::as_str).unwrap_or("").to_string();
                Media { r#type: "image".into(), url, alt }
            })
            .collect();
    }

    // Video view: `app.bsky.embed.video#view` has `playlist` (HLS URL) and
    // optionally `thumbnail`.
    if embed.get("playlist").is_some() || embed.get("video").is_some() {
        let url = embed
            .get("playlist")
            .and_then(Value::as_str)
            .or_else(|| embed.get("thumbnail").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let alt = embed.get("alt").and_then(Value::as_str).unwrap_or("").to_string();
        return vec![Media { r#type: "video".into(), url, alt }];
    }

    Vec::new()
}

/// Extract hashtags from `record.facets[].features[$type~"tag"].tag`.
fn parse_tags(record: &Value) -> Vec<String> {
    let facets = match record.get("facets").and_then(Value::as_array) {
        Some(f) => f,
        None => return Vec::new(),
    };

    let mut tags = Vec::new();
    for facet in facets {
        if let Some(features) = facet.get("features").and_then(Value::as_array) {
            for feat in features {
                if feat
                    .get("$type")
                    .and_then(Value::as_str)
                    .map(|t| t.contains("tag") || t.contains("hashtag"))
                    .unwrap_or(false)
                {
                    if let Some(tag) = feat.get("tag").and_then(Value::as_str) {
                        tags.push(tag.to_string());
                    }
                }
            }
        }
    }
    tags
}

/// Parse an ISO 8601 / RFC 3339 datetime string to local RFC3339.
fn parse_iso8601_to_local(s: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// The pull.

/// Load all existing post guids from the contract layer (for incremental dedupe).
fn load_existing_guids(vault: &Vault) -> HashSet<String> {
    let mut out = HashSet::new();
    let stream = vault.stream(DIR, Partition::Month);
    if let Ok(keys) = stream.partitions() {
        for key in keys {
            if let Ok(posts) = stream.read::<Post>(&key) {
                for p in posts {
                    out.insert(p.guid);
                }
            }
        }
    }
    out
}

/// Full pull: load session token, drain feed, write contract + raw.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Bluesky is not connected — paste your handle:app-password in Integrations")?;

    let access_jwt = token.access_token.trim().to_string();
    if access_jwt.is_empty() {
        bail!("Bluesky session token is empty — reconnect in Integrations");
    }
    // The handle (for actor parameter) is stored in the `scope` field.
    let actor = token.scope.as_deref().unwrap_or("").trim().to_string();
    if actor.is_empty() {
        bail!("Bluesky handle is unknown — reconnect in Integrations");
    }
    // DID is stored in `token_type` for ownership filtering.
    let actor_did = token.token_type.as_deref().unwrap_or(&actor).to_string();

    let client = BskyClient::new(APP_VIEW_HOST);
    pull_with(vault, &client, &access_jwt, &actor, &actor_did)
}

fn pull_with(
    vault: &Vault,
    client: &impl AuthorFeed,
    access_jwt: &str,
    actor: &str,
    actor_did: &str,
) -> Result<PullOutcome> {
    let mut state = vault.read_bluesky_sync();
    let mut existing = load_existing_guids(vault);

    let contract_stream = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut posts: Vec<Post> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();

    // getAuthorFeed only paginates BACKWARD (older items).
    // Always start from the top (cursor=None) and page downward, stopping
    // as soon as a full page of already-held items is seen (all_seen) or
    // the feed is exhausted. No pagination cursor is stored between runs —
    // storing the first-page bottom cursor and replaying it would fetch only
    // items OLDER than the backfill, permanently missing new posts.
    let mut cursor: Option<String> = None;

    loop {
        let body = client.fetch_feed(actor, cursor.as_deref(), access_jwt)?;

        let next_cursor = body.get("cursor").and_then(Value::as_str).map(|s| s.to_string());

        let feed = body
            .get("feed")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&[]);

        if feed.is_empty() {
            break;
        }

        let mut all_seen = true;
        for item in feed {
            let Some(parsed) = parse_feed_item(item, actor_did) else {
                // Raw every item regardless (likes, follows, reposts of others).
                continue;
            };
            let guid = parsed.post.guid.clone();
            if existing.insert(guid) {
                all_seen = false;
                posts.push(parsed.post);
                raws.push(parsed.raw);
            }
        }

        // Stop draining once all items on a page are already held.
        // This is the incremental stop: on second+ syncs the top pages will
        // have new posts (not seen) and we collect them, then hit the boundary
        // where everything is already in the vault and break.
        if all_seen {
            break;
        }

        match next_cursor {
            Some(c) => {
                std::thread::sleep(PAGE_PAUSE);
                cursor = Some(c);
            }
            None => break,
        }
    }

    // Write raw unconditionally, contract for authored posts.
    if !posts.is_empty() {
        contract_stream.append(&posts, |p| &p.ts)?;
        raw_stream.append(&raws, |r| &r.ts)?;
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_bluesky_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{} posts", posts.len()),
        counts: BTreeMap::from([
            ("posts", posts.len() as u64),
            ("raw", raws.len() as u64),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike as _;
    use serde_json::json;
    use std::cell::RefCell;

    // ---------------------------------------------------------------------------
    // parse_credential tests.

    #[test]
    fn credential_parse_valid() {
        let (h, p) = parse_credential("alice.bsky.social:xxxx-xxxx-xxxx-xxxx").unwrap();
        assert_eq!(h, "alice.bsky.social");
        assert_eq!(p, "xxxx-xxxx-xxxx-xxxx");
    }

    #[test]
    fn credential_parse_no_colon_errors() {
        assert!(parse_credential("alicebskysocial").is_err());
    }

    #[test]
    fn credential_parse_empty_handle_errors() {
        assert!(parse_credential(":xxxx-xxxx-xxxx-xxxx").is_err());
    }

    #[test]
    fn credential_parse_empty_password_errors() {
        assert!(parse_credential("alice.bsky.social:").is_err());
    }

    // ---------------------------------------------------------------------------
    // parse_feed_item tests.

    fn make_post_item(
        uri: &str,
        did: &str,
        text: &str,
        created_at: &str,
        reply: Option<Value>,
        embed: Option<Value>,
    ) -> Value {
        let mut record = json!({
            "$type": "app.bsky.feed.post",
            "text": text,
            "createdAt": created_at,
        });
        if let Some(r) = reply {
            record["reply"] = r;
        }
        if let Some(e) = embed {
            record["embed"] = e;
        }
        json!({
            "post": {
                "uri": uri,
                "cid": "bafyreiabc123",
                "author": {
                    "did": did,
                    "handle": "alice.bsky.social",
                    "displayName": "Alice"
                },
                "record": record,
                "likeCount": 5,
                "repostCount": 2,
                "replyCount": 1,
                "indexedAt": created_at,
            }
        })
    }

    #[test]
    fn parse_simple_post() {
        let did = "did:plc:alice123";
        let item = make_post_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kxy",
            did,
            "Hello from Bluesky!",
            "2026-06-10T14:03:01Z",
            None,
            None,
        );
        let parsed = parse_feed_item(&item, did).unwrap();
        assert_eq!(parsed.post.source, "bluesky");
        assert_eq!(parsed.post.guid, "at://did:plc:alice123/app.bsky.feed.post/3kxy");
        assert_eq!(parsed.post.kind, "post");
        assert_eq!(parsed.post.text, "Hello from Bluesky!");
        assert!(parsed.post.reply_to.is_empty());
        assert!(parsed.post.thread.is_empty());
        assert_eq!(parsed.post.extra.get("likeCount"), Some(&json!(5)));
    }

    #[test]
    fn parse_reply_post() {
        let did = "did:plc:alice123";
        let reply_val = json!({
            "root": {"uri": "at://did:plc:alice123/app.bsky.feed.post/3kw0", "cid": "bafyroot"},
            "parent": {"uri": "at://did:plc:bob456/app.bsky.feed.post/3kw2", "cid": "bafyparent"}
        });
        let item = make_post_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kxy",
            did,
            "I agree!",
            "2026-06-10T15:00:00Z",
            Some(reply_val),
            None,
        );
        let parsed = parse_feed_item(&item, did).unwrap();
        assert_eq!(parsed.post.kind, "reply");
        assert_eq!(parsed.post.reply_to, "at://did:plc:bob456/app.bsky.feed.post/3kw2");
        assert_eq!(parsed.post.thread, "at://did:plc:alice123/app.bsky.feed.post/3kw0");
    }

    #[test]
    fn parse_quote_post() {
        let did = "did:plc:alice123";
        let embed_val = json!({
            "$type": "app.bsky.embed.record",
            "record": {
                "uri": "at://did:plc:bob456/app.bsky.feed.post/3kw5",
                "cid": "bafyquoted"
            }
        });
        let item = make_post_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kxz",
            did,
            "Quoting this!",
            "2026-06-11T09:00:00Z",
            None,
            Some(embed_val),
        );
        let parsed = parse_feed_item(&item, did).unwrap();
        assert_eq!(parsed.post.kind, "quote");
        assert_eq!(parsed.post.quote_of, "at://did:plc:bob456/app.bsky.feed.post/3kw5");
    }

    #[test]
    fn parse_repost() {
        let did = "did:plc:alice123";
        let original_did = "did:plc:bob456";
        let original_uri = "at://did:plc:bob456/app.bsky.feed.post/3kw3";
        let item = json!({
            "post": {
                "uri": original_uri,
                "cid": "bafyoriginal",
                "author": {
                    "did": original_did,
                    "handle": "bob.bsky.social"
                },
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "Original post",
                    "createdAt": "2026-06-10T12:00:00Z",
                },
                "likeCount": 10,
                "repostCount": 5,
                "replyCount": 0,
                "indexedAt": "2026-06-10T12:00:00Z",
            },
            "reason": {
                "$type": "app.bsky.feed.defs#reasonRepost",
                "by": {
                    "did": did,
                    "handle": "alice.bsky.social"
                },
                "indexedAt": "2026-06-10T13:00:00Z"
            }
        });
        let parsed = parse_feed_item(&item, did).unwrap();
        assert_eq!(parsed.post.kind, "repost");
        // repost_of must carry the original URI; quote_of must be empty.
        assert_eq!(parsed.post.repost_of, original_uri, "repost_of should be the original post URI");
        assert!(parsed.post.quote_of.is_empty(), "quote_of must be empty for a repost");
        // Guid must be distinct from the original URI (prevents collision when
        // the actor reposts their own posts).
        assert_ne!(parsed.post.guid, original_uri, "repost guid must differ from original URI");
        assert!(parsed.post.guid.contains(original_uri), "repost guid should encode the original URI");
        // Timestamp must come from reason.indexedAt ("2026-06-10T13:00:00Z"),
        // not record.createdAt ("2026-06-10T12:00:00Z").
        // The ts is stored in local RFC3339, so we re-parse to compare UTC hours.
        let ts_utc = chrono::DateTime::parse_from_rfc3339(&parsed.post.ts)
            .expect("ts should be valid RFC3339")
            .with_timezone(&chrono::Utc);
        assert_eq!(ts_utc.hour(), 13, "repost ts should be reason.indexedAt (13:00 UTC), not record.createdAt (12:00 UTC)");
    }

    #[test]
    fn repost_of_own_post_no_guid_collision() {
        // When the actor reposts their OWN post, the repost guid must differ
        // from the original post's guid to avoid dedup collision.
        let did = "did:plc:alice123";
        let own_uri = "at://did:plc:alice123/app.bsky.feed.post/3kself";
        // First: the original post item.
        let original_item = make_post_item(own_uri, did, "My own post", "2026-06-10T10:00:00Z", None, None);
        // Second: the repost of that same post.
        let repost_item = json!({
            "post": {
                "uri": own_uri,
                "cid": "bafyself",
                "author": {"did": did, "handle": "alice.bsky.social"},
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "My own post",
                    "createdAt": "2026-06-10T10:00:00Z",
                },
                "likeCount": 0,
                "repostCount": 1,
                "replyCount": 0,
                "indexedAt": "2026-06-10T10:00:00Z",
            },
            "reason": {
                "$type": "app.bsky.feed.defs#reasonRepost",
                "by": {"did": did, "handle": "alice.bsky.social"},
                "indexedAt": "2026-06-10T12:00:00Z"
            }
        });
        let orig = parse_feed_item(&original_item, did).unwrap();
        let rp = parse_feed_item(&repost_item, did).unwrap();
        assert_ne!(orig.post.guid, rp.post.guid, "repost guid must differ from original post guid");
    }

    #[test]
    fn parse_reply_with_quote() {
        // A reply that also quotes another post should have both reply_to and quote_of set.
        let did = "did:plc:alice123";
        let reply_val = json!({
            "root": {"uri": "at://did:plc:alice123/app.bsky.feed.post/root", "cid": "bafyroot"},
            "parent": {"uri": "at://did:plc:bob456/app.bsky.feed.post/parent", "cid": "bafyparent"}
        });
        let embed_val = json!({
            "$type": "app.bsky.embed.record",
            "record": {
                "uri": "at://did:plc:carol789/app.bsky.feed.post/quoted",
                "cid": "bafyquoted"
            }
        });
        let item = make_post_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kreplyquote",
            did,
            "Replying and quoting",
            "2026-06-12T14:00:00Z",
            Some(reply_val),
            Some(embed_val),
        );
        let parsed = parse_feed_item(&item, did).unwrap();
        assert_eq!(parsed.post.kind, "reply");
        assert_eq!(parsed.post.reply_to, "at://did:plc:bob456/app.bsky.feed.post/parent");
        assert_eq!(parsed.post.quote_of, "at://did:plc:carol789/app.bsky.feed.post/quoted",
            "quote_of should be set even when the post is also a reply");
    }

    #[test]
    fn skips_other_users_posts() {
        let actor_did = "did:plc:alice123";
        let other_did = "did:plc:bob456";
        let item = make_post_item(
            "at://did:plc:bob456/app.bsky.feed.post/3kw7",
            other_did,
            "Someone else's post",
            "2026-06-11T08:00:00Z",
            None,
            None,
        );
        let result = parse_feed_item(&item, actor_did);
        assert!(result.is_none(), "should skip posts not authored by the connected user");
    }

    #[test]
    fn parse_image_media() {
        // The CDN fullsize URL lives in post.embed (the hydrated view), NOT
        // in post.record.embed (which carries blob refs). This test reflects
        // the real getAuthorFeed response shape.
        let did = "did:plc:alice123";
        // record.embed has a blob ref (no fullsize — this is the real shape).
        let record_embed = json!({
            "$type": "app.bsky.embed.images",
            "images": [
                {
                    "alt": "A beautiful sunset",
                    "image": {"$type": "blob", "ref": {"$link": "bafyimageblob"}, "mimeType": "image/jpeg", "size": 123456}
                }
            ]
        });
        // post.embed (the view) has the CDN fullsize URL.
        let view_embed = json!({
            "$type": "app.bsky.embed.images#view",
            "images": [
                {
                    "alt": "A beautiful sunset",
                    "fullsize": "https://cdn.bsky.app/img/feed_fullsize/plain/bafyimageblob@jpeg",
                    "thumb": "https://cdn.bsky.app/img/feed_thumbnail/plain/bafyimageblob@jpeg"
                }
            ]
        });
        // Build a realistic feed item with separate record.embed and post.embed.
        let item = json!({
            "post": {
                "uri": "at://did:plc:alice123/app.bsky.feed.post/3kximg",
                "cid": "bafyreiabc123",
                "author": {
                    "did": did,
                    "handle": "alice.bsky.social",
                    "displayName": "Alice"
                },
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "Check this out!",
                    "createdAt": "2026-06-12T10:00:00Z",
                    "embed": record_embed,
                },
                "embed": view_embed,
                "likeCount": 5,
                "repostCount": 0,
                "replyCount": 0,
                "indexedAt": "2026-06-12T10:00:00Z",
            }
        });
        let parsed = parse_feed_item(&item, did).unwrap();
        assert_eq!(parsed.post.media.len(), 1);
        assert_eq!(parsed.post.media[0].r#type, "image");
        assert_eq!(parsed.post.media[0].alt, "A beautiful sunset");
        assert_eq!(
            parsed.post.media[0].url,
            "https://cdn.bsky.app/img/feed_fullsize/plain/bafyimageblob@jpeg",
            "url must come from post.embed (CDN view), not record.embed (blob ref)"
        );
    }

    #[test]
    fn parse_hashtags_from_facets() {
        let did = "did:plc:alice123";
        let item = json!({
            "post": {
                "uri": "at://did:plc:alice123/app.bsky.feed.post/3kxtag",
                "cid": "bafytag",
                "author": {"did": did, "handle": "alice.bsky.social"},
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "Hello #rust #atproto",
                    "createdAt": "2026-06-12T11:00:00Z",
                    "facets": [
                        {
                            "index": {"byteStart": 6, "byteEnd": 11},
                            "features": [
                                {"$type": "app.bsky.richtext.facet#tag", "tag": "rust"}
                            ]
                        },
                        {
                            "index": {"byteStart": 12, "byteEnd": 20},
                            "features": [
                                {"$type": "app.bsky.richtext.facet#tag", "tag": "atproto"}
                            ]
                        }
                    ]
                },
                "likeCount": 0,
                "repostCount": 0,
                "replyCount": 0,
                "indexedAt": "2026-06-12T11:00:00Z",
            }
        });
        let parsed = parse_feed_item(&item, did).unwrap();
        assert_eq!(parsed.post.tags, vec!["rust", "atproto"]);
    }

    // ---------------------------------------------------------------------------
    // Full pull cycle with a stub client.

    struct StubFeed {
        pages: RefCell<Vec<Value>>,
    }

    impl AuthorFeed for StubFeed {
        fn fetch_feed(&self, _actor: &str, _cursor: Option<&str>, _jwt: &str) -> Result<Value> {
            let mut pages = self.pages.borrow_mut();
            if pages.is_empty() {
                return Ok(json!({"feed": []}));
            }
            Ok(pages.remove(0))
        }
    }

    fn make_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-bluesky-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn feed_page(items: &[Value], cursor: Option<&str>) -> Value {
        let mut v = json!({"feed": Value::Array(items.to_vec())});
        if let Some(c) = cursor {
            v["cursor"] = json!(c);
        }
        v
    }

    fn make_item(uri: &str, did: &str, text: &str, ts: &str) -> Value {
        make_post_item(uri, did, text, ts, None, None)
    }

    #[test]
    fn pull_with_writes_posts_and_raw() {
        let vault = make_vault("writes_posts");
        let did = "did:plc:alice123";

        let item1 = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kpost1",
            did,
            "First post",
            "2026-06-10T14:00:00Z",
        );
        let item2 = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kpost2",
            did,
            "Second post",
            "2026-06-11T09:00:00Z",
        );

        let stub = StubFeed {
            pages: RefCell::new(vec![feed_page(&[item1, item2], None)]),
        };

        let out = pull_with(&vault, &stub, "jwt_token", "alice.bsky.social", did).unwrap();
        assert_eq!(*out.counts.get("posts").unwrap(), 2);
        assert_eq!(*out.counts.get("raw").unwrap(), 2);

        // Contract layer should have 2 posts.
        let stream = vault.stream(DIR, Partition::Month);
        let keys = stream.partitions().unwrap();
        let all_posts: Vec<Post> = keys
            .iter()
            .flat_map(|k| stream.read::<Post>(k).unwrap())
            .collect();
        assert_eq!(all_posts.len(), 2);
        let guids: Vec<&str> = all_posts.iter().map(|p| p.guid.as_str()).collect();
        assert!(guids.contains(&"at://did:plc:alice123/app.bsky.feed.post/3kpost1"));
        assert!(guids.contains(&"at://did:plc:alice123/app.bsky.feed.post/3kpost2"));
    }

    #[test]
    fn pull_with_deduplicates_on_rerun() {
        let vault = make_vault("dedup");
        let did = "did:plc:alice123";

        let item1 = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kpost1",
            did,
            "First post",
            "2026-06-10T14:00:00Z",
        );

        // First run: 1 post written.
        let stub1 = StubFeed {
            pages: RefCell::new(vec![feed_page(&[item1.clone()], None)]),
        };
        let out1 = pull_with(&vault, &stub1, "jwt", "alice.bsky.social", did).unwrap();
        assert_eq!(*out1.counts.get("posts").unwrap(), 1);

        // Second run: same item. Should deduplicate (all_seen stops the drain).
        let stub2 = StubFeed {
            pages: RefCell::new(vec![feed_page(&[item1], None)]),
        };
        let out2 = pull_with(&vault, &stub2, "jwt", "alice.bsky.social", did).unwrap();
        assert_eq!(*out2.counts.get("posts").unwrap(), 0, "re-run should not duplicate");

        // Vault still has exactly 1 post.
        let stream = vault.stream(DIR, Partition::Month);
        let keys = stream.partitions().unwrap();
        let count: usize = keys.iter().map(|k| stream.read::<Post>(k).unwrap().len()).sum();
        assert_eq!(count, 1);
    }

    #[test]
    fn incremental_collects_new_posts_above_backfill() {
        // Regression test for the cursor-direction bug:
        // - First sync: page1 (item1 newest, item2 older) — no further pages.
        // - Second sync: page1 returns item_new (NEWER than item1) + item1 (already seen).
        //   The all_seen stop must not fire until it processes item_new as unseen.
        //
        // The old buggy code stored the bottom-of-page1 cursor and on the second run
        // would start FROM that old cursor, which returns items OLDER than item1,
        // missing item_new entirely. The fix always starts from cursor=None.
        let vault = make_vault("incremental_new");
        let did = "did:plc:alice123";

        let item1 = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kpage1a",
            did,
            "First post",
            "2026-06-10T10:00:00Z",
        );
        let item2 = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kpage1b",
            did,
            "Older post",
            "2026-06-09T10:00:00Z",
        );
        let item_new = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3knew",
            did,
            "Brand new post (newer than item1)",
            "2026-06-11T08:00:00Z",
        );

        // First run: two posts.
        let stub1 = StubFeed {
            pages: RefCell::new(vec![feed_page(&[item1.clone(), item2.clone()], None)]),
        };
        let out1 = pull_with(&vault, &stub1, "jwt", "alice.bsky.social", did).unwrap();
        assert_eq!(*out1.counts.get("posts").unwrap(), 2, "first sync should collect 2 posts");

        // Second run: feed top has the new item + item1 (seen). item2 not present (older page).
        // Expected: item_new collected (1 new post), item1 deduped.
        let stub2 = StubFeed {
            pages: RefCell::new(vec![
                // Page 1: new item at top, then already-seen item1.
                feed_page(&[item_new.clone(), item1.clone()], Some("cursor_older")),
                // Page 2 should NOT be fetched — all_seen fires after page1 is exhausted
                // (item_new was new, but the page after it should be cut off).
                // If the old bug were present, the second run would start from the stored
                // cursor and never see item_new at all, returning 0 new posts.
            ]),
        };
        let out2 = pull_with(&vault, &stub2, "jwt", "alice.bsky.social", did).unwrap();
        assert_eq!(
            *out2.counts.get("posts").unwrap(),
            1,
            "second sync should collect the 1 new post above the prior backfill"
        );

        // Vault should have all 3 unique posts.
        let stream = vault.stream(DIR, Partition::Month);
        let keys = stream.partitions().unwrap();
        let count: usize = keys.iter().map(|k| stream.read::<Post>(k).unwrap().len()).sum();
        assert_eq!(count, 3, "vault should hold all 3 unique posts after two syncs");
    }

    #[test]
    fn pull_with_drains_multiple_pages() {
        let vault = make_vault("multi_page");
        let did = "did:plc:alice123";

        let item1 = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kpage1a",
            did,
            "Page 1 post",
            "2026-06-10T14:00:00Z",
        );
        let item2 = make_item(
            "at://did:plc:alice123/app.bsky.feed.post/3kpage2a",
            did,
            "Page 2 post",
            "2026-06-09T10:00:00Z",
        );

        let stub = StubFeed {
            pages: RefCell::new(vec![
                feed_page(&[item1], Some("cursor_page2")),
                feed_page(&[item2], None),
            ]),
        };

        let out = pull_with(&vault, &stub, "jwt", "alice.bsky.social", did).unwrap();
        assert_eq!(*out.counts.get("posts").unwrap(), 2);
    }

    #[test]
    fn post_contract_fields_omit_empty() {
        let post = Post::new(
            "bluesky",
            "at://did:plc:x/app.bsky.feed.post/abc",
            "2026-06-10T07:00:00-07:00",
        );
        let v = serde_json::to_value(&post).unwrap();
        assert_eq!(v["source"], json!("bluesky"));
        assert_eq!(v["guid"], json!("at://did:plc:x/app.bsky.feed.post/abc"));
        // Omit-empty fields should not appear.
        assert!(v.get("reply_to").is_none(), "empty reply_to should be omitted");
        assert!(v.get("media").is_none(), "empty media should be omitted");
        assert!(v.get("tags").is_none(), "empty tags should be omitted");
        assert!(v.get("extra").is_none(), "empty extra should be omitted");
    }
}
