//! Mastodon (Fediverse) — periodic REST pull from the user's home instance,
//! paired with an optional archive ZIP import for full-history backfill.
//!
//! ## Auth & connection
//!
//! Mastodon OAuth requires dynamic app registration per instance (each
//! instance is its own authorization server), which cannot be handled by the
//! static [`crate::sync::oauth::Provider`] model. Instead, the user generates
//! a personal access token directly in their instance's web UI:
//!
//! > Settings → Development → New Application → check `read:statuses`,
//! > `read:accounts`, `read:favourites` → Your Access Token
//!
//! They paste `https://instance.example|token` as a single compound string.
//!
//! ## Two integrations, one module
//!
//! Following the Google many-defs-one-module pattern:
//! - `DEF` — Periodic poller (30-minute cadence). Reads
//!   `GET /api/v1/accounts/verify_credentials` (to get the account id) then
//!   pages `GET /api/v1/accounts/:id/statuses` with a `max_id` cursor.
//!   Cursor is stored in `.trove/mastodon-sync.json` and advanced only after
//!   a full drain (crash-safe).
//! - `IMPORT_DEF` — Import for the archive ZIP (Settings → Import and Export
//!   → Request archive). Parses `outbox.json` (ActivityStreams 2.0 JSON-LD).
//!   Requestable every 7 days; pairs with the API for freshness.
//!
//! ## Two layers (unconditional)
//!
//! - **Raw**: `social/mastodon/raw/YYYY-MM.jsonl` — API status objects or
//!   archive Activity objects, full fidelity.
//! - **Contract**: `social/mastodon/YYYY-MM.jsonl` — one [`crate::social::Post`]
//!   per authored item. `guid = status URI` (stable, cross-path deduplication
//!   between API and archive paths).
//!
//! Boosts (reblogs) are emitted as repost contract rows + raw.
//! Direct-message statuses (visibility == "direct") are excluded from the
//! social/ contract stream — they would belong to correspondence/, which is
//! not yet implemented.  All other authored statuses are emitted as contract rows.
//!
//! ## Cursor
//!
//! `GET /api/v1/accounts/:id/statuses` paginates downward (older items) via
//! `max_id`. On first run: start with no `max_id`, drain until exhausted.
//! On subsequent runs: start with no `max_id` again (to catch new posts at
//! the top), page until we hit an `all_seen` page (a full page of already-
//! held guids) — then stop. The persisted `max_id` records the oldest status
//! id we've written; it is the stopping hint on re-runs (once we see IDs
//! older than our oldest we can stop).
//!
//! ## Archive import
//!
//! `outbox.json` is an ActivityStreams 2.0 `OrderedCollection` whose
//! `orderedItems` are `Create`/`Announce` Activity objects. We parse
//! `type=Create` (the user's own posts) as contract rows and `type=Announce`
//! (boosts) as raw only. Dedupe is via the same URI/guid key so an API poll
//! after a prior archive import produces zero duplicates.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef, ImportOutcome,
    ImportParam, ImportSpec, IntegrationDef, PullOutcome,
};
use crate::social::{Media, Post};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "mastodon";
const DIR: &str = "social/mastodon";
const RAW_DIR: &str = "social/mastodon/raw";
const SYNC_FILE: &str = ".trove/mastodon-sync.json";
const SERVICE: &str = "mastodon";

const PAGE_LIMIT: u64 = 40;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE_PAUSE: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Account id on the instance (avoids re-fetching verify_credentials
    /// on every sync — cached after first successful connect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    /// The lowest (oldest) status ID written to disk on this instance.
    /// Informational only — the actual stop condition is guid-set based
    /// (all_seen full page), which is robust to gaps.  Not used as an
    /// early-stop hint in the current implementation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oldest_id: Option<String>,
    /// RFC3339 local time of the last successful sync (display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_mastodon_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_mastodon_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Registry face — Periodic DEF.

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
            "mastodon sync skipped: not connected",
        ));
    }

    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("posts").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("mastodon synced — {n} posts")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "mastodon sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let posts = out.counts.get("posts").copied().unwrap_or(0);
    let headline = if posts == 0 {
        "Mastodon is up to date — no new posts".to_string()
    } else {
        format!("Mastodon synced — {posts} new posts")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Periodic pull def — registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "mastodon",
        name: "Mastodon",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Mastodon posts and boosts from your home instance via the \
                      Mastodon REST API. Paste your instance URL and a personal access token \
                      to connect. Polls every 30 minutes for new posts; use the archive import \
                      (7-day cadence) for full history.",
        domain: "social",
        vault_path: "social/mastodon/",
        toggleable: true,
        setup: &[
            "In Mastodon: Settings → Development → New Application. Enable scopes \
             read:statuses, read:accounts, read:favourites. Save, then copy Your Access Token.",
            "Paste your instance URL and access token as: https://your.instance|your_token",
        ],
        caveats: "Followers list is not included in the archive export. Direct messages \
                  (visibility: direct) are excluded from the social stream; they would \
                  belong to correspondence/ which is not yet implemented. Boosts are stored \
                  as repost contract rows. The archive ZIP is requestable every 7 days; use \
                  Import to backfill history.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(1800),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("mastodon"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Archive import def.

fn import_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Archive import def — registered in [`crate::integrations::INTEGRATIONS`].
pub static IMPORT_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "mastodon-archive",
        name: "Mastodon Archive",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your full Mastodon post history from the official archive export \
                      (Settings → Import and Export → Request your archive). Writes to the same \
                      social/mastodon/ stream as the API sync — re-importing never duplicates.",
        domain: "social",
        vault_path: "social/mastodon/",
        toggleable: false,
        setup: &[
            "Mastodon → Settings → Import and Export → Export → Request your archive. A ZIP \
             download link will arrive by notification (usually within minutes).",
            "Drop the ZIP here. Posts dedupe with any previously synced API data.",
        ],
        caveats: "The archive is requestable every 7 days. Media files in the archive are \
                  never copied — only their paths are recorded as metadata.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(import_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[ImportParam {
        key: "instance",
        label: "Instance URL (optional)",
        placeholder: "https://mastodon.social (leave blank if unknown)",
        required: false,
    }],
    run: run_archive_import,
};

// ---------------------------------------------------------------------------
// Connection — TokenPaste: `https://instance.example|access_token`.

fn def_connect(vault: &Vault, cred: &str) -> Result<()> {
    let (instance_url, token) = parse_credential(cred.trim())?;

    // Verify the token works and get the account id.
    let account_id = verify_credentials(instance_url, token)?;

    // Store: access_token = the bearer token, scope = instance URL,
    // token_type = account_id (reusing the field for our purposes).
    let token_set = crate::sync::oauth::TokenSet {
        access_token: token.to_string(),
        refresh_token: None,
        token_type: Some(account_id.clone()),
        scope: Some(instance_url.to_string()),
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &token_set)?;

    // Warm the sync state with the account id so we don't re-fetch it.
    let mut state = vault.read_mastodon_sync();
    state.account_id = Some(account_id);
    vault.write_mastodon_sync(&state)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        // scope = instance URL.
        let instance = token.scope.as_deref().unwrap_or("unknown instance").to_string();
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: instance,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "mastodon",
    display_name: "Mastodon",
    methods: &[ConnectMethod::TokenPaste {
        label: "Instance URL and Access Token",
        help: "Generate a personal access token in your Mastodon instance: \
               Settings → Development → New Application. Enable scopes \
               read:statuses, read:accounts, read:favourites. Copy 'Your Access Token'. \
               Then paste your instance URL and the token separated by | \
               (e.g. https://mastodon.social|xxxxxxxxxxxxxxxxxxxxxx).",
        placeholder: "https://mastodon.social|your_access_token",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["mastodon"],
    setup: &[
        "In Mastodon: Settings → Development → New Application.",
        "Enable scopes: read:statuses, read:accounts, read:favourites. Submit.",
        "Copy 'Your Access Token' from the app detail page.",
        "Paste as https://your.instance|your_token here.",
    ],
};

// ---------------------------------------------------------------------------
// Credential helpers.

/// Parse `https://instance.example|access_token`. The instance URL may
/// contain colons (https scheme), so we split on `|` (not `:`).
fn parse_credential(cred: &str) -> Result<(&str, &str)> {
    let idx = cred.find('|').ok_or_else(|| {
        anyhow::anyhow!(
            "expected instance_url|access_token \
             (e.g. https://mastodon.social|xxxxxxxxxxxxxxxxxxxxxx)"
        )
    })?;
    let instance = cred[..idx].trim();
    let token = cred[idx + 1..].trim();
    if instance.is_empty() {
        bail!("instance URL is empty — expected https://your.instance|token");
    }
    if !instance.starts_with("http://") && !instance.starts_with("https://") {
        bail!("instance URL must start with http:// or https://");
    }
    if token.is_empty() {
        bail!("access token is empty — expected https://your.instance|token");
    }
    Ok((instance, token))
}

// ---------------------------------------------------------------------------
// HTTP helpers.

/// Verify credentials and return the account id.
fn verify_credentials(instance_url: &str, token: &str) -> Result<String> {
    let url = format!("{instance_url}/api/v1/accounts/verify_credentials");
    let resp = ureq::get(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("verify_credentials: {e}"))?;
    if resp.status() == 401 {
        bail!("Mastodon token rejected — check your access token and instance URL");
    }
    if resp.status() != 200 {
        bail!("verify_credentials: HTTP {}", resp.status());
    }
    let v: Value = resp.into_json().context("verify_credentials: parse error")?;
    let id = v.get("id").and_then(Value::as_str).context("missing 'id' in verify_credentials")?;
    Ok(id.to_string())
}

/// Fetch one page of statuses for `account_id` on `instance_url`.
/// `max_id`: if Some, returns statuses older than (but not including) that id.
fn fetch_statuses(
    instance_url: &str,
    token: &str,
    account_id: &str,
    max_id: Option<&str>,
) -> Result<Value> {
    let url = format!("{instance_url}/api/v1/accounts/{account_id}/statuses");
    let mut req = ureq::get(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .query("limit", &PAGE_LIMIT.to_string())
        // exclude_reblogs=false to get boosts too (we differentiate them below)
        .query("exclude_reblogs", "false");
    if let Some(id) = max_id {
        req = req.query("max_id", id);
    }
    let resp = req.call().map_err(|e| anyhow::anyhow!("statuses: {e}"))?;
    if resp.status() == 401 {
        bail!("Mastodon session expired — reconnect in Integrations");
    }
    if resp.status() != 200 {
        bail!("statuses: HTTP {}", resp.status());
    }
    let v: Value = resp.into_json().context("statuses: parse error")?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Parsing — API status objects.

/// Raw line: the full status object with a ts used only for partitioning.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Parse a Mastodon boost (reblog) status into a contract Post (kind="repost")
/// + raw line.  The wrapper status URI is the guid (already unique).
/// Returns None when the status is not a boost or timestamps are missing.
fn parse_boost(status: &Value) -> Option<(Post, RawLine)> {
    let id = status.get("id").and_then(Value::as_str)?;
    let created_at = status.get("created_at").and_then(Value::as_str)?;
    let ts = parse_mastodon_ts(created_at)?;

    let reblog = status.get("reblog")?;
    if reblog.is_null() {
        return None;
    }

    // Wrapper status URI is our guid (the boost action itself has a unique URI).
    let guid = status.get("uri").and_then(Value::as_str).unwrap_or(id).to_string();

    // The original post's URI is repost_of.
    let repost_of = reblog
        .get("uri")
        .and_then(Value::as_str)
        .or_else(|| reblog.get("url").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    let raw = RawLine { ts: ts.clone(), value: status.clone() };

    let mut post = Post::new(SOURCE, guid, ts);
    post.kind = "repost".to_string();
    post.repost_of = repost_of;

    Some((post, raw))
}

/// Parse a Mastodon API status object into a contract Post + raw line.
/// Returns None for boosts (those are handled by parse_boost), for
/// direct-message statuses (visibility == "direct", which belong to
/// correspondence/ not social/ — excluded for now), and items with
/// unparseable timestamps.
fn parse_status(status: &Value) -> Option<(Post, RawLine)> {
    let id = status.get("id").and_then(Value::as_str)?;
    let created_at = status.get("created_at").and_then(Value::as_str)?;
    let ts = parse_mastodon_ts(created_at)?;

    // guid = the canonical URI (stable, cross-source deduplication).
    let guid = status.get("uri").and_then(Value::as_str).unwrap_or(id).to_string();

    let raw = RawLine { ts: ts.clone(), value: status.clone() };

    // Reblogs (boosts): handled by parse_boost, not here.
    let reblog = status.get("reblog");
    if reblog.map(|v| !v.is_null()).unwrap_or(false) {
        return None;
    }

    // Direct-message statuses (visibility == "direct"): the social/ contract
    // explicitly excludes DMs — they belong to correspondence/, not the post
    // stream. Exclude them here (raw still written by the pull loop caller).
    let visibility = status.get("visibility").and_then(Value::as_str).unwrap_or("");
    if visibility == "direct" {
        return None;
    }

    let text = html_to_text(
        status.get("content").and_then(Value::as_str).unwrap_or(""),
    );

    // in_reply_to_id → reply_to (the parent status id, not full URI).
    let reply_to = status
        .get("in_reply_to_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let kind = if !reply_to.is_empty() { "reply" } else { "post" }.to_string();

    let lang = status.get("language").and_then(Value::as_str).unwrap_or("").to_string();

    let url = status.get("url").and_then(Value::as_str).unwrap_or("").to_string();

    // Visibility → context (Mastodon-native grouping label).
    let context = status.get("visibility").and_then(Value::as_str).unwrap_or("").to_string();

    // Tags — `tags[].name`.
    let tags: Vec<String> = status
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Media attachments.
    let media: Vec<Media> = status
        .get("media_attachments")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(parse_media_attachment).collect())
        .unwrap_or_default();

    // Extra: engagement counts + sensitive flag.
    let mut extra = Map::new();
    for key in ["replies_count", "reblogs_count", "favourites_count"] {
        if let Some(n) = status.get(key).and_then(Value::as_i64) {
            extra.insert(key.to_string(), Value::Number(n.into()));
        }
    }
    if let Some(s) = status.get("sensitive").and_then(Value::as_bool) {
        if s {
            extra.insert("sensitive".into(), Value::Bool(true));
        }
    }
    let spoiler = status.get("spoiler_text").and_then(Value::as_str).unwrap_or("");
    if !spoiler.is_empty() {
        extra.insert("spoiler_text".into(), Value::String(spoiler.to_string()));
    }

    let mut post = Post::new(SOURCE, guid, ts);
    post.kind = kind;
    post.text = text;
    post.lang = lang;
    post.url = url;
    post.context = context;
    post.reply_to = reply_to;
    post.tags = tags;
    post.media = media;
    post.extra = extra;

    Some((post, raw))
}

/// Parse a `media_attachments` item.
fn parse_media_attachment(att: &Value) -> Media {
    // `type` values: unknown, image, gifv, video, audio.
    let att_type = att.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string();
    // `url` is the CDN/public URL; `preview_url` is thumbnail. Prefer `url`.
    let url = att
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // `description` is the alt text.
    let alt = att.get("description").and_then(Value::as_str).unwrap_or("").to_string();
    Media { r#type: att_type, url, alt }
}

/// Lightly strip HTML from Mastodon content (content is HTML; we want plain
/// text for the vault). Strips tags, collapses whitespace, decodes minimal
/// entities. Not a full sanitizer — close enough for the vault text field.
fn html_to_text(html: &str) -> String {
    // Replace block-level tags with spaces.
    let mut s = html.to_string();
    for tag in &["<br>", "<br/>", "<br />", "</p>", "</div>"] {
        s = s.replace(tag, " ");
    }
    // Strip all remaining tags.
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // Decode minimal HTML entities.
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    // Collapse runs of whitespace.
    let mut result = String::with_capacity(out.len());
    let mut last_space = true;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !last_space {
                result.push(' ');
            }
            last_space = true;
        } else {
            result.push(ch);
            last_space = false;
        }
    }
    result.trim().to_string()
}

/// Parse a Mastodon `created_at` ISO 8601 UTC string → local RFC3339.
fn parse_mastodon_ts(s: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Load existing guids for incremental dedupe.

/// Load all guids already held in the vault (both contract rows and raw boost
/// rows) so that neither authored posts nor boosts are duplicated on re-runs.
///
/// Contract posts contribute their `guid` directly.
/// Raw boost rows (API path: `reblog != null`; archive path: `type == Announce`)
/// contribute a `boost:<guid>` key so the pull loop's `existing.insert(…)`
/// check survives across process restarts.
fn load_existing_guids(vault: &Vault) -> HashSet<String> {
    let mut out = HashSet::new();

    // --- Contract layer (authored posts + repost contract rows) ---
    let stream = vault.stream(DIR, Partition::Month);
    if let Ok(keys) = stream.partitions() {
        for key in keys {
            if let Ok(posts) = stream.read::<Post>(&key) {
                for p in posts {
                    out.insert(p.guid.clone());
                    // If this is a repost contract row, also register its
                    // boost: prefix so the pull loop's is_boost branch finds it.
                    if p.kind == "repost" {
                        out.insert(format!("boost:{}", p.guid));
                    }
                }
            }
        }
    }

    // --- Raw layer (scan for any boost rows not yet promoted to contract) ---
    // We read the raw JSONL as generic Values so we don't need a typed struct.
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    if let Ok(keys) = raw_stream.partitions() {
        for key in keys {
            if let Ok(raws) = raw_stream.read::<Value>(&key) {
                for raw in raws {
                    // API boost: `reblog` field is non-null.
                    let is_api_boost = raw
                        .get("reblog")
                        .map(|v| !v.is_null())
                        .unwrap_or(false);
                    // Archive boost: `type == Announce`.
                    let is_archive_announce = raw
                        .get("type")
                        .and_then(Value::as_str)
                        .map(|t| t.eq_ignore_ascii_case("Announce"))
                        .unwrap_or(false);

                    if is_api_boost || is_archive_announce {
                        // Extract the wrapper/activity URI as the boost key.
                        let guid = raw
                            .get("uri")
                            .or_else(|| raw.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !guid.is_empty() {
                            out.insert(format!("boost:{guid}"));
                        }
                    }
                }
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// The periodic pull.

/// Full pull: load token, drain statuses, write contract + raw.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token_set = vault
        .load_sync_token(SERVICE)?
        .context("Mastodon is not connected — paste your instance_url|token in Integrations")?;

    let access_token = token_set.access_token.trim().to_string();
    if access_token.is_empty() {
        bail!("Mastodon token is empty — reconnect in Integrations");
    }
    let instance_url = token_set
        .scope
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    if instance_url.is_empty() {
        bail!("Mastodon instance URL is unknown — reconnect in Integrations");
    }

    // Resolve account id: use cached value or re-fetch.
    let mut state = vault.read_mastodon_sync();
    let account_id = if let Some(id) = &state.account_id {
        id.clone()
    } else {
        let id = verify_credentials(&instance_url, &access_token)?;
        state.account_id = Some(id.clone());
        id
    };

    let mut existing = load_existing_guids(vault);

    let contract_stream = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut posts: Vec<Post> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();

    // Always start from the top (no max_id) and page backward.
    // Stop when we get an empty page OR when a full page is all-seen.
    let mut max_id: Option<String> = None;
    let mut oldest_seen_id: Option<String> = None;

    loop {
        let body = fetch_statuses(
            &instance_url,
            &access_token,
            &account_id,
            max_id.as_deref(),
        )?;

        let statuses = body.as_array().map(Vec::as_slice).unwrap_or(&[]);
        if statuses.is_empty() {
            break;
        }

        let mut all_seen = true;
        let mut page_oldest_id: Option<String> = None;

        for status in statuses {
            let id = status.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            page_oldest_id = Some(id.clone());

            // guid = URI for deduplication across API and archive paths.
            let guid = status
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();

            // Boosts (reblog != null): emit a contract repost row + raw.
            // load_existing_guids seeds the `boost:` keys from prior runs so
            // this insert returns false (already held) for seen boosts.
            let is_boost = status
                .get("reblog")
                .map(|v| !v.is_null())
                .unwrap_or(false);

            if is_boost {
                if existing.insert(format!("boost:{guid}")) {
                    all_seen = false;
                    // parse_boost returns None only if ts is missing; skip that
                    // row rather than letting an empty ts crash the partition.
                    if let Some((post, raw)) = parse_boost(status) {
                        posts.push(post);
                        raws.push(raw);
                    }
                }
                continue;
            }

            if existing.insert(guid.clone()) {
                all_seen = false;
                if let Some((post, raw)) = parse_status(status) {
                    posts.push(post);
                    raws.push(raw);
                }
            }
        }

        // Advance oldest id tracking.
        if let Some(oid) = page_oldest_id {
            oldest_seen_id = Some(oid.clone());
            max_id = Some(oid);
        }

        // Stop once a full page is already held — we've reached our prior backfill.
        if all_seen {
            break;
        }

        std::thread::sleep(PAGE_PAUSE);
    }

    // Write — raw first, then contract.
    // Write — raw first, then contract.
    if !posts.is_empty() || !raws.is_empty() {
        if !posts.is_empty() {
            contract_stream.append(&posts, |p| &p.ts)?;
        }
        if !raws.is_empty() {
            raw_stream.append(&raws, |r| &r.ts)?;
        }
    }

    // Persist cursor state. Advance oldest_id only when we wrote something new.
    if let Some(oid) = oldest_seen_id {
        state.oldest_id = Some(oid);
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_mastodon_sync(&state)?;

    let n_posts = posts.len() as u64;
    let n_raw = raws.len() as u64;
    Ok(PullOutcome {
        headline: format!("{n_posts} posts (incl. reposts), {n_raw} raw items"),
        counts: BTreeMap::from([("posts", n_posts), ("raw", n_raw)]),
    })
}

// ---------------------------------------------------------------------------
// Archive import — ActivityStreams 2.0 JSON-LD (outbox.json).

/// Parse an ActivityStreams `Create` activity from `outbox.json` into a
/// `Post`. Returns `None` when the activity has no usable timestamp.
fn parse_archive_create(activity: &Value) -> Option<Post> {
    // Only `type=Create` activities are the user's own authored statuses.
    let typ = activity.get("type").and_then(Value::as_str)?;
    if !typ.eq_ignore_ascii_case("Create") {
        return None;
    }

    // The actual status object is in `object`.
    let obj = activity.get("object")?;

    // `published` on the activity (prefer) or on the object.
    let published = activity
        .get("published")
        .and_then(Value::as_str)
        .or_else(|| obj.get("published").and_then(Value::as_str))?;

    let ts = parse_mastodon_ts(published)?;

    // guid = the status `id` (the URI: `https://instance/users/acct/statuses/N`).
    let guid = obj.get("id").and_then(Value::as_str)?;

    // `content` or `contentMap` (language-keyed content, pick any value).
    let content = obj
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("contentMap")
                .and_then(Value::as_object)
                .and_then(|m| m.values().next())
                .and_then(Value::as_str)
        })
        .unwrap_or("");

    let text = html_to_text(content);

    // `inReplyTo` is a URI string (or null).
    let reply_to = obj
        .get("inReplyTo")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = if !reply_to.is_empty() { "reply" } else { "post" }.to_string();

    // `url` — the web permalink.
    let url = obj.get("url").and_then(Value::as_str).unwrap_or("").to_string();

    // `tag` array — ActivityStreams tags (Hashtag / Mention).
    let tags: Vec<String> = obj
        .get("tag")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|t| {
                    t.get("type")
                        .and_then(Value::as_str)
                        .map(|ty| ty == "Hashtag")
                        .unwrap_or(false)
                })
                .filter_map(|t| {
                    t.get("name")
                        .and_then(Value::as_str)
                        // strip leading # from hashtag names
                        .map(|n| n.trim_start_matches('#').to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    // `attachment` — media.
    let media: Vec<Media> = obj
        .get("attachment")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|att| {
                    let att_type = att
                        .get("mediaType")
                        .and_then(Value::as_str)
                        .map(|mt| {
                            if mt.starts_with("image/") {
                                "image"
                            } else if mt.starts_with("video/") {
                                "video"
                            } else if mt.starts_with("audio/") {
                                "audio"
                            } else {
                                "unknown"
                            }
                        })
                        .unwrap_or("unknown")
                        .to_string();
                    let att_url = att.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                    let alt = att
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Media { r#type: att_type, url: att_url, alt }
                })
                .collect()
        })
        .unwrap_or_default();

    // `to` / `cc` → Mastodon-native visibility token.
    // https://www.w3.org/TR/activitypub/#public-addressing
    // In Mastodon archives these fields live on the Activity; fall back to
    // the inner Note object if absent on the Activity.
    //
    // Mapping (mirrors Mastodon server logic):
    //   to contains Public                        → "public"
    //   cc contains Public (but not to)           → "unlisted"
    //   to contains followers collection only     → "private" (followers-only)
    //   neither Public nor followers in to/cc     → "direct"
    //
    // Direct-message activities (context == "direct") are excluded from the
    // social/ stream — return None so the caller skips the contract row.
    let context_opt: Option<&str> = {
        let empty = vec![];
        let to_act = activity.get("to").and_then(Value::as_array).unwrap_or(&empty);
        let cc_act = activity.get("cc").and_then(Value::as_array).unwrap_or(&empty);
        let to_obj = obj.get("to").and_then(Value::as_array).unwrap_or(&empty);
        let cc_obj = obj.get("cc").and_then(Value::as_array).unwrap_or(&empty);

        const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
        const FOLLOWERS_SUFFIX: &str = "/followers";

        let in_to = |s: &str| {
            to_act.iter().chain(to_obj.iter()).any(|v| v.as_str() == Some(s))
        };
        let in_cc = |s: &str| {
            cc_act.iter().chain(cc_obj.iter()).any(|v| v.as_str() == Some(s))
        };
        let has_followers_in_to = to_act
            .iter()
            .chain(to_obj.iter())
            .any(|v| v.as_str().map(|s| s.ends_with(FOLLOWERS_SUFFIX)).unwrap_or(false));

        if in_to(PUBLIC) {
            Some("public")
        } else if in_cc(PUBLIC) {
            Some("unlisted")
        } else if has_followers_in_to {
            Some("private")
        } else {
            // Addressed to specific actors only — this is a DM.
            // Exclude from social/ (same policy as the API path).
            None
        }
    };
    let Some(context) = context_opt else {
        return None;
    };
    let context = context.to_string();

    let mut post = Post::new(SOURCE, guid.to_string(), ts);
    post.kind = kind;
    post.text = text;
    post.url = url;
    post.context = context;
    post.reply_to = reply_to;
    post.tags = tags;
    post.media = media;

    Some(post)
}

/// Parse an ActivityStreams `Announce` activity from `outbox.json` into a
/// contract Post (kind="repost").  The activity `id` (URI) is the guid.
/// Returns `None` when the activity has no usable timestamp.
fn parse_archive_announce(activity: &Value) -> Option<Post> {
    let typ = activity.get("type").and_then(Value::as_str)?;
    if !typ.eq_ignore_ascii_case("Announce") {
        return None;
    }

    let published = activity.get("published").and_then(Value::as_str)?;
    let ts = parse_mastodon_ts(published)?;

    // The activity `id` is the boost wrapper URI (unique per boost action).
    let guid = activity.get("id").and_then(Value::as_str)?;

    // `object` is either a plain URI string or an object with an `id`.
    let repost_of = activity
        .get("object")
        .and_then(|o| o.as_str().or_else(|| o.get("id").and_then(Value::as_str)))
        .unwrap_or("")
        .to_string();

    let mut post = Post::new(SOURCE, guid.to_string(), ts);
    post.kind = "repost".to_string();
    post.repost_of = repost_of;

    Some(post)
}

fn run_archive_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load existing guids for dedupe — both contract rows and raw Announce rows
    // so re-importing the same ZIP never appends duplicate boosts.
    let contract_stream = vault.stream(DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract_stream.partitions()? {
        for p in contract_stream.read::<Post>(&key)? {
            if !p.guid.is_empty() {
                seen.insert(p.guid);
            }
        }
    }

    // Also scan raw for Announce activities not yet in the contract layer.
    let raw_stream_load = vault.stream(RAW_DIR, Partition::Month);
    if let Ok(keys) = raw_stream_load.partitions() {
        for key in keys {
            if let Ok(raws) = raw_stream_load.read::<Value>(&key) {
                for raw in raws {
                    let is_announce = raw
                        .get("type")
                        .and_then(Value::as_str)
                        .map(|t| t.eq_ignore_ascii_case("Announce"))
                        .unwrap_or(false);
                    if is_announce {
                        if let Some(id) = raw.get("id").and_then(Value::as_str) {
                            seen.insert(id.to_string());
                        }
                    }
                }
            }
        }
    }

    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file).with_context(|| {
        format!(
            "reading {} — is this a Mastodon archive ZIP?",
            path.display()
        )
    })?;

    // Find outbox.json — collect names first to avoid borrow conflict.
    let entry_names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|e| e.name().to_string()))
        .collect();

    let outbox_name = entry_names
        .iter()
        .find(|n| {
            let normalized = n.replace('\\', "/");
            normalized == "outbox.json" || normalized.ends_with("/outbox.json")
        })
        .cloned();

    let Some(outbox_name) = outbox_name else {
        bail!("outbox.json not found in ZIP — is this a Mastodon archive?");
    };

    let mut body = String::new();
    zip.by_name(&outbox_name)?.read_to_string(&mut body)?;

    let root: Value = serde_json::from_str(&body).context("outbox.json: parse error")?;

    // The outbox is an `OrderedCollection` with `orderedItems`.
    let items = root
        .get("orderedItems")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let total = items.len();
    let mut posts: Vec<Post> = Vec::new();
    let mut raw_lines: Vec<(String, Value)> = Vec::new(); // (ts, value)
    let mut duplicates: u64 = 0;
    let mut boosts: u64 = 0;

    for (i, activity) in items.iter().enumerate() {
        // Progress every 100 items.
        if i % 100 == 0 && total > 0 {
            progress(ImportProgress {
                records: posts.len() as u64,
                percent: (i as f32 / total as f32) * 90.0,
            });
        }

        let typ = activity.get("type").and_then(Value::as_str).unwrap_or("");

        // Boost (Announce) — emit a contract repost row + raw, with dedupe.
        if typ.eq_ignore_ascii_case("Announce") {
            if let Some(post) = parse_archive_announce(activity) {
                if seen.insert(post.guid.clone()) {
                    boosts += 1;
                    let ts = post.ts.clone();
                    raw_lines.push((ts, activity.clone()));
                    posts.push(post);
                } else {
                    duplicates += 1;
                }
            }
            // Skip Announces with no usable timestamp (parse_archive_announce
            // returned None) rather than propagating an empty ts.
            continue;
        }

        let Some(post) = parse_archive_create(activity) else {
            continue;
        };

        if !seen.insert(post.guid.clone()) {
            duplicates += 1;
            continue;
        }

        // Raw: the full activity object.
        let ts = post.ts.clone();
        raw_lines.push((ts, activity.clone()));
        posts.push(post);
    }

    // Write.
    if !posts.is_empty() {
        contract_stream.append(&posts, |p| &p.ts)?;
    }

    // Write raw lines (both Creates and Announces).
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    if !raw_lines.is_empty() {
        #[derive(Serialize)]
        struct RawArchiveLine<'a> {
            #[serde(skip)]
            ts: &'a str,
            #[serde(flatten)]
            value: &'a Value,
        }
        let raw_items: Vec<RawArchiveLine> = raw_lines
            .iter()
            .map(|(ts, v)| RawArchiveLine { ts: ts.as_str(), value: v })
            .collect();
        raw_stream.append(&raw_items, |r| r.ts)?;
    }

    progress(ImportProgress { records: posts.len() as u64, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{} posts imported, {} boosts (reposts), {} duplicates skipped",
            posts.len(),
            boosts,
            duplicates
        ),
        counts: [
            ("posts", posts.len() as u64),
            ("boosts", boosts),
            ("duplicates", duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-mastodon-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -------------------------------------------------------------------------
    // parse_credential tests.

    #[test]
    fn credential_parse_valid() {
        let (inst, tok) = parse_credential("https://mastodon.social|mytoken123").unwrap();
        assert_eq!(inst, "https://mastodon.social");
        assert_eq!(tok, "mytoken123");
    }

    #[test]
    fn credential_parse_no_pipe_errors() {
        assert!(parse_credential("https://mastodon.social:mytoken").is_err());
    }

    #[test]
    fn credential_parse_empty_instance_errors() {
        assert!(parse_credential("|mytoken").is_err());
    }

    #[test]
    fn credential_parse_empty_token_errors() {
        assert!(parse_credential("https://mastodon.social|").is_err());
    }

    #[test]
    fn credential_parse_non_http_errors() {
        assert!(parse_credential("mastodon.social|token").is_err());
    }

    // -------------------------------------------------------------------------
    // html_to_text tests.

    #[test]
    fn html_strip_basic_tags() {
        assert_eq!(html_to_text("<p>Hello world</p>"), "Hello world");
    }

    #[test]
    fn html_strip_links_keep_text() {
        let html = r#"<p>Check <a href="https://example.com">this</a> out</p>"#;
        assert_eq!(html_to_text(html), "Check this out");
    }

    #[test]
    fn html_decode_entities() {
        // Double-encoded: &amp;lt; → first &amp; → & giving &lt;
        assert_eq!(html_to_text("&amp;lt; &gt;"), "< >");
        // Single &amp; decodes to &.
        assert_eq!(html_to_text("&amp;"), "&");
        // &lt; / &gt; decode to < / >.
        assert_eq!(html_to_text("&lt;code&gt;"), "<code>");
    }

    #[test]
    fn html_br_becomes_space() {
        assert_eq!(html_to_text("line1<br>line2"), "line1 line2");
        assert_eq!(html_to_text("line1<br/>line2"), "line1 line2");
    }

    #[test]
    fn html_empty_is_empty() {
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("   "), "");
    }

    // -------------------------------------------------------------------------
    // parse_mastodon_ts tests.

    #[test]
    fn ts_parse_utc() {
        let ts = parse_mastodon_ts("2024-06-15T10:30:00Z").unwrap();
        // Should be valid RFC3339 (offset may vary by TZ, but parseable).
        let dt = chrono::DateTime::parse_from_rfc3339(&ts).unwrap();
        assert_eq!(dt.with_timezone(&chrono::Utc).timestamp(), 1718447400);
    }

    #[test]
    fn ts_parse_invalid_returns_none() {
        assert!(parse_mastodon_ts("not-a-date").is_none());
        assert!(parse_mastodon_ts("").is_none());
    }

    // -------------------------------------------------------------------------
    // parse_status tests (API layer).

    fn api_status(id: &str, uri: &str, text_html: &str, created_at: &str) -> Value {
        json!({
            "id": id,
            "uri": uri,
            "created_at": created_at,
            "content": text_html,
            "visibility": "public",
            "language": "en",
            "in_reply_to_id": null,
            "reblog": null,
            "url": format!("https://mastodon.social/@alice/{id}"),
            "sensitive": false,
            "spoiler_text": "",
            "media_attachments": [],
            "tags": [{"name": "rust", "url": "https://mastodon.social/tags/rust"}],
            "replies_count": 2,
            "reblogs_count": 5,
            "favourites_count": 10
        })
    }

    #[test]
    fn parse_status_plain_post() {
        let s = api_status(
            "112345678901234567",
            "https://mastodon.social/users/alice/statuses/112345678901234567",
            "<p>Hello #rust world</p>",
            "2024-06-15T10:30:00Z",
        );
        let (post, _raw) = parse_status(&s).unwrap();
        assert_eq!(post.source, "mastodon");
        assert_eq!(post.guid, "https://mastodon.social/users/alice/statuses/112345678901234567");
        assert_eq!(post.kind, "post");
        assert_eq!(post.text, "Hello #rust world");
        assert_eq!(post.lang, "en");
        assert_eq!(post.context, "public");
        assert_eq!(post.tags, vec!["rust"]);
        assert!(post.reply_to.is_empty());
        assert_eq!(post.extra.get("reblogs_count"), Some(&json!(5)));
        assert_eq!(post.extra.get("favourites_count"), Some(&json!(10)));
    }

    #[test]
    fn parse_status_reply() {
        let mut s = api_status(
            "112345678901234568",
            "https://mastodon.social/users/alice/statuses/112345678901234568",
            "<p>I agree!</p>",
            "2024-06-15T11:00:00Z",
        );
        s["in_reply_to_id"] = json!("112345678901234000");
        let (post, _raw) = parse_status(&s).unwrap();
        assert_eq!(post.kind, "reply");
        assert_eq!(post.reply_to, "112345678901234000");
    }

    #[test]
    fn parse_status_boost_returns_none() {
        let mut s = api_status(
            "112345678901234569",
            "https://mastodon.social/users/alice/statuses/112345678901234569",
            "",
            "2024-06-15T12:00:00Z",
        );
        s["reblog"] = json!({"id": "999", "content": "<p>original</p>"});
        // Boosts are raw-only; parse_status returns None.
        assert!(parse_status(&s).is_none());
    }

    #[test]
    fn parse_status_with_media() {
        let mut s = api_status(
            "112345678901234570",
            "https://mastodon.social/users/alice/statuses/112345678901234570",
            "<p>Check this photo</p>",
            "2024-06-15T13:00:00Z",
        );
        s["media_attachments"] = json!([
            {
                "id": "22345678",
                "type": "image",
                "url": "https://files.mastodon.social/media_attachments/files/000/001/234/original/abc.jpg",
                "preview_url": "https://files.mastodon.social/media_attachments/files/000/001/234/small/abc.jpg",
                "description": "A sunset over the ocean",
                "blurhash": "UACl,M00IV4:~qxu"
            }
        ]);
        let (post, _raw) = parse_status(&s).unwrap();
        assert_eq!(post.media.len(), 1);
        assert_eq!(post.media[0].r#type, "image");
        assert_eq!(post.media[0].alt, "A sunset over the ocean");
        assert!(post.media[0].url.contains("original/abc.jpg"));
    }

    #[test]
    fn parse_status_sensitive_goes_to_extra() {
        let mut s = api_status(
            "112345678901234571",
            "https://mastodon.social/users/alice/statuses/112345678901234571",
            "<p>CW post</p>",
            "2024-06-15T14:00:00Z",
        );
        s["sensitive"] = json!(true);
        s["spoiler_text"] = json!("content warning");
        let (post, _raw) = parse_status(&s).unwrap();
        assert_eq!(post.extra.get("sensitive"), Some(&json!(true)));
        assert_eq!(post.extra.get("spoiler_text"), Some(&json!("content warning")));
    }

    #[test]
    fn parse_status_direct_visibility_excluded() {
        // API-path: DMs (visibility == "direct") must not produce a contract row.
        let mut s = api_status(
            "112345678901234599",
            "https://mastodon.social/users/alice/statuses/112345678901234599",
            "<p>secret DM</p>",
            "2024-06-15T15:00:00Z",
        );
        s["visibility"] = json!("direct");
        // parse_status must return None so the post is not emitted to social/.
        assert!(
            parse_status(&s).is_none(),
            "direct-visibility statuses must be excluded from the social/ stream"
        );
    }

    #[test]
    fn parse_status_public_and_unlisted_accepted() {
        // Public statuses: accepted.
        let s_pub = api_status(
            "112345678901234600",
            "https://mastodon.social/users/alice/statuses/112345678901234600",
            "<p>public post</p>",
            "2024-06-15T15:01:00Z",
        );
        let (post, _) = parse_status(&s_pub).unwrap();
        assert_eq!(post.context, "public");

        // Unlisted: accepted.
        let mut s_un = api_status(
            "112345678901234601",
            "https://mastodon.social/users/alice/statuses/112345678901234601",
            "<p>unlisted post</p>",
            "2024-06-15T15:02:00Z",
        );
        s_un["visibility"] = json!("unlisted");
        let (post2, _) = parse_status(&s_un).unwrap();
        assert_eq!(post2.context, "unlisted");
    }

    // -------------------------------------------------------------------------
    // parse_archive_create tests.

    fn archive_create(uri: &str, content: &str, published: &str) -> Value {
        json!({
            "type": "Create",
            "id": format!("https://mastodon.social/users/alice/statuses/{uri}/activity"),
            "published": published,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://mastodon.social/users/alice/followers"],
            "object": {
                "id": format!("https://mastodon.social/users/alice/statuses/{uri}"),
                "type": "Note",
                "published": published,
                "url": format!("https://mastodon.social/@alice/{uri}"),
                "content": content,
                "inReplyTo": null,
                "attachment": [],
                "tag": [
                    {"type": "Hashtag", "name": "#rustlang", "href": "https://mastodon.social/tags/rustlang"},
                    {"type": "Mention", "name": "@bob@example.com", "href": "https://example.com/users/bob"}
                ]
            }
        })
    }

    #[test]
    fn archive_create_parses_post() {
        let a = archive_create(
            "112345678901234567",
            "<p>Hello from the archive! #rustlang</p>",
            "2024-06-15T10:30:00Z",
        );
        let post = parse_archive_create(&a).unwrap();
        assert_eq!(post.source, "mastodon");
        assert_eq!(
            post.guid,
            "https://mastodon.social/users/alice/statuses/112345678901234567"
        );
        assert_eq!(post.kind, "post");
        assert_eq!(post.text, "Hello from the archive! #rustlang");
        // Tags: only Hashtag, not Mention; # stripped.
        assert_eq!(post.tags, vec!["rustlang"]);
        // Public audience → "public" context.
        assert_eq!(post.context, "public");
    }

    #[test]
    fn archive_create_with_reply() {
        let mut a = archive_create(
            "112345678901234568",
            "<p>Replying to you</p>",
            "2024-06-15T11:00:00Z",
        );
        a["object"]["inReplyTo"] =
            json!("https://mastodon.social/users/alice/statuses/112345678901234000");
        let post = parse_archive_create(&a).unwrap();
        assert_eq!(post.kind, "reply");
        assert_eq!(
            post.reply_to,
            "https://mastodon.social/users/alice/statuses/112345678901234000"
        );
    }

    #[test]
    fn archive_create_context_unlisted() {
        // Public in cc (not to) → "unlisted" native token.
        let a = json!({
            "type": "Create",
            "id": "https://mastodon.social/users/alice/statuses/unlisted1/activity",
            "published": "2024-06-15T10:00:00Z",
            "to": ["https://mastodon.social/users/alice/followers"],
            "cc": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": {
                "id": "https://mastodon.social/users/alice/statuses/unlisted1",
                "type": "Note",
                "published": "2024-06-15T10:00:00Z",
                "url": "https://mastodon.social/@alice/unlisted1",
                "content": "<p>Unlisted post</p>",
                "inReplyTo": null,
                "attachment": [],
                "tag": []
            }
        });
        let post = parse_archive_create(&a).unwrap();
        assert_eq!(post.context, "unlisted");
    }

    #[test]
    fn archive_create_context_private_followers_only() {
        // Followers collection in to, no Public anywhere → "private".
        let a = json!({
            "type": "Create",
            "id": "https://mastodon.social/users/alice/statuses/priv1/activity",
            "published": "2024-06-15T10:00:00Z",
            "to": ["https://mastodon.social/users/alice/followers"],
            "cc": [],
            "object": {
                "id": "https://mastodon.social/users/alice/statuses/priv1",
                "type": "Note",
                "published": "2024-06-15T10:00:00Z",
                "url": "https://mastodon.social/@alice/priv1",
                "content": "<p>Followers-only post</p>",
                "inReplyTo": null,
                "attachment": [],
                "tag": []
            }
        });
        let post = parse_archive_create(&a).unwrap();
        assert_eq!(post.context, "private");
    }

    #[test]
    fn archive_create_direct_excluded_from_contract() {
        // DM: addressed to a specific actor inbox only (no Public, no followers).
        // parse_archive_create must return None — DMs don't belong in social/.
        let a = json!({
            "type": "Create",
            "id": "https://mastodon.social/users/alice/statuses/dm1/activity",
            "published": "2024-06-15T10:00:00Z",
            "to": ["https://other.social/users/bob"],
            "cc": [],
            "object": {
                "id": "https://mastodon.social/users/alice/statuses/dm1",
                "type": "Note",
                "published": "2024-06-15T10:00:00Z",
                "url": "https://mastodon.social/@alice/dm1",
                "content": "<p>Private DM</p>",
                "inReplyTo": null,
                "attachment": [],
                "tag": []
            }
        });
        assert!(
            parse_archive_create(&a).is_none(),
            "direct-addressed archive activities must be excluded from social/"
        );
    }

    #[test]
    fn archive_announce_returns_none() {
        let a = json!({
            "type": "Announce",
            "id": "https://mastodon.social/users/alice/statuses/112/activity",
            "published": "2024-06-15T12:00:00Z",
            "object": "https://other.instance/users/bob/statuses/999"
        });
        // parse_archive_create only handles Create.
        assert!(parse_archive_create(&a).is_none());
    }

    #[test]
    fn archive_create_media_attachment() {
        let mut a = archive_create(
            "112345678901234569",
            "<p>A photo</p>",
            "2024-06-15T13:00:00Z",
        );
        a["object"]["attachment"] = json!([{
            "type": "Document",
            "mediaType": "image/jpeg",
            "url": "https://files.mastodon.social/media_attachments/files/000/000/001/original/abc.jpg",
            "name": "A beautiful landscape"
        }]);
        let post = parse_archive_create(&a).unwrap();
        assert_eq!(post.media.len(), 1);
        assert_eq!(post.media[0].r#type, "image");
        assert_eq!(post.media[0].alt, "A beautiful landscape");
        assert!(post.media[0].url.contains("original/abc.jpg"));
    }

    // -------------------------------------------------------------------------
    // Archive import integration test.

    fn make_outbox_zip_named(name: &str, items: &[Value]) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-mastodon-archive-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        let outbox = json!({
            "@context": ["https://www.w3.org/ns/activitystreams"],
            "type": "OrderedCollection",
            "id": "https://mastodon.social/users/alice/outbox",
            "totalItems": items.len(),
            "orderedItems": items
        });
        z.start_file("outbox.json", opts).unwrap();
        z.write_all(serde_json::to_string(&outbox).unwrap().as_bytes()).unwrap();
        z.finish().unwrap();
        path
    }

    #[test]
    fn archive_import_writes_posts_and_raw() {
        let vault = temp_vault("archive_import");
        let create1 = archive_create(
            "111000000000000001",
            "<p>First post</p>",
            "2024-05-01T08:00:00Z",
        );
        let create2 = archive_create(
            "111000000000000002",
            "<p>Second post with &amp; entity</p>",
            "2024-06-01T09:00:00Z",
        );
        let announce = json!({
            "type": "Announce",
            "id": "https://mastodon.social/users/alice/statuses/boost1/activity",
            "published": "2024-06-01T10:00:00Z",
            "object": "https://other.social/users/bob/statuses/999"
        });

        let zip_path = make_outbox_zip_named("import_writes", &[create1, create2, announce]);
        let outcome = (IMPORT.run)(
            &vault,
            &zip_path,
            &BTreeMap::new(),
            &mut |_| {},
        )
        .unwrap();

        // posts = 3 (2 Creates + 1 Announce repost); boosts = 1; duplicates = 0.
        assert_eq!(outcome.counts.get("posts"), Some(&3));
        assert_eq!(outcome.counts.get("boosts"), Some(&1));
        assert_eq!(outcome.counts.get("duplicates"), Some(&0));

        // Contract file for May — 1 row (the Create).
        let may_path = vault.root().join("social/mastodon/2024-05.jsonl");
        let may = fs::read_to_string(&may_path).unwrap();
        let may_posts: Vec<Post> =
            may.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(may_posts.len(), 1);
        assert_eq!(may_posts[0].text, "First post");
        assert_eq!(may_posts[0].source, "mastodon");

        // Contract file for June — 2 rows (one Create post + one Announce repost).
        let jun_path = vault.root().join("social/mastodon/2024-06.jsonl");
        let jun = fs::read_to_string(&jun_path).unwrap();
        let jun_posts: Vec<Post> =
            jun.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(jun_posts.len(), 2, "June should have a Create + an Announce repost");
        let jun_create = jun_posts.iter().find(|p| p.kind == "post" || p.kind == "reply").unwrap();
        assert_eq!(jun_create.text, "Second post with & entity");
        let jun_repost = jun_posts.iter().find(|p| p.kind == "repost").unwrap();
        assert_eq!(jun_repost.repost_of, "https://other.social/users/bob/statuses/999");

        // Raw dir should exist and have lines (all activities go to raw).
        let raw_dir = vault.root().join("social/mastodon/raw");
        assert!(raw_dir.exists());

        let _ = fs::remove_file(zip_path);
    }

    #[test]
    fn archive_import_deduplicates_on_reimport() {
        let vault = temp_vault("archive_dedup");
        let create1 = archive_create(
            "111000000000000010",
            "<p>Dedup post</p>",
            "2024-06-10T08:00:00Z",
        );

        let zip_path = make_outbox_zip_named("dedup", &[create1]);

        // First import.
        let out1 = (IMPORT.run)(&vault, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out1.counts.get("posts"), Some(&1));

        // Second import: same content.
        let out2 = (IMPORT.run)(&vault, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out2.counts.get("posts"), Some(&0), "re-import should not duplicate");
        assert_eq!(out2.counts.get("duplicates"), Some(&1));

        // File unchanged — exactly 1 line.
        let content =
            fs::read_to_string(vault.root().join("social/mastodon/2024-06.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 1);

        let _ = fs::remove_file(zip_path);
    }

    #[test]
    fn archive_import_missing_outbox_errors() {
        let vault = temp_vault("archive_no_outbox");
        let path = std::env::temp_dir()
            .join(format!("trove-mastodon-nooutbox-{}.zip", std::process::id()));
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("actor.json", opts).unwrap();
            z.write_all(b"{}").unwrap();
            z.finish().unwrap();
        }
        let result = (IMPORT.run)(&vault, &path, &BTreeMap::new(), &mut |_| {});
        assert!(result.is_err(), "missing outbox.json should error");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn post_contract_fields_omit_empty() {
        let post = Post::new("mastodon", "https://example.com/users/x/statuses/1", "2024-06-15T10:00:00+00:00");
        let v = serde_json::to_value(&post).unwrap();
        assert_eq!(v["source"], json!("mastodon"));
        assert!(v.get("reply_to").is_none(), "empty reply_to should be omitted");
        assert!(v.get("media").is_none(), "empty media should be omitted");
        assert!(v.get("tags").is_none(), "empty tags should be omitted");
        assert!(v.get("extra").is_none(), "empty extra should be omitted");
    }

    #[test]
    fn old_post_lines_still_deserialize() {
        // Back-compat: sparse lines written by future versions are tolerated.
        let line = r#"{"ts":"2024-06-15T10:00:00-07:00","source":"mastodon","guid":"https://example.social/users/x/statuses/1","unknown_future_field":"x"}"#;
        let p: Post = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "https://example.social/users/x/statuses/1");
        assert_eq!(p.kind, "");
        assert!(p.text.is_empty());
    }

    // -------------------------------------------------------------------------
    // parse_boost tests (API layer).

    #[test]
    fn parse_boost_emits_repost_contract_row() {
        let mut s = api_status(
            "112345678901234569",
            "https://mastodon.social/users/alice/statuses/112345678901234569",
            "",
            "2024-06-15T12:00:00Z",
        );
        let original_uri = "https://other.social/users/bob/statuses/99999";
        s["reblog"] = json!({
            "id": "99999",
            "uri": original_uri,
            "content": "<p>original post</p>",
            "url": "https://other.social/@bob/99999"
        });
        // parse_status still returns None for boosts.
        assert!(parse_status(&s).is_none(), "parse_status must not handle boosts");
        // parse_boost emits a repost row.
        let (post, raw) = parse_boost(&s).unwrap();
        assert_eq!(post.kind, "repost");
        assert_eq!(post.guid, "https://mastodon.social/users/alice/statuses/112345678901234569");
        assert_eq!(post.repost_of, original_uri);
        assert!(post.text.is_empty(), "repost rows carry no text");
        // Raw carries the full status object.
        assert!(raw.value.get("reblog").is_some());
    }

    #[test]
    fn parse_boost_no_reblog_field_returns_none() {
        let s = api_status(
            "112345678901234570",
            "https://mastodon.social/users/alice/statuses/112345678901234570",
            "<p>plain post</p>",
            "2024-06-15T12:00:00Z",
        );
        assert!(parse_boost(&s).is_none());
    }

    // -------------------------------------------------------------------------
    // parse_archive_announce tests.

    #[test]
    fn parse_archive_announce_emits_repost() {
        let a = json!({
            "type": "Announce",
            "id": "https://mastodon.social/users/alice/statuses/boost42/activity",
            "published": "2024-06-15T12:00:00Z",
            "object": "https://other.social/users/bob/statuses/999"
        });
        let post = parse_archive_announce(&a).unwrap();
        assert_eq!(post.kind, "repost");
        assert_eq!(post.guid, "https://mastodon.social/users/alice/statuses/boost42/activity");
        assert_eq!(post.repost_of, "https://other.social/users/bob/statuses/999");
        assert!(post.text.is_empty());
    }

    #[test]
    fn parse_archive_announce_object_as_object_uses_id() {
        // Some Mastodon archives embed the boosted status as a full object.
        let a = json!({
            "type": "Announce",
            "id": "https://mastodon.social/users/alice/statuses/boost43/activity",
            "published": "2024-06-15T13:00:00Z",
            "object": {
                "id": "https://other.social/users/carol/statuses/77777",
                "type": "Note",
                "content": "<p>hi</p>"
            }
        });
        let post = parse_archive_announce(&a).unwrap();
        assert_eq!(post.kind, "repost");
        assert_eq!(post.repost_of, "https://other.social/users/carol/statuses/77777");
    }

    #[test]
    fn parse_archive_announce_missing_published_returns_none() {
        let a = json!({
            "type": "Announce",
            "id": "https://mastodon.social/users/alice/statuses/boost44/activity",
            "object": "https://other.social/users/bob/statuses/999"
        });
        assert!(parse_archive_announce(&a).is_none());
    }

    // -------------------------------------------------------------------------
    // Boost deduplication across runs: archive reimport must not duplicate Announces.

    #[test]
    fn archive_import_deduplicates_announces_on_reimport() {
        let vault = temp_vault("archive_dedup_announce");
        let create1 = archive_create(
            "111000000000000020",
            "<p>Post for boost dedup test</p>",
            "2024-06-10T08:00:00Z",
        );
        let announce = json!({
            "type": "Announce",
            "id": "https://mastodon.social/users/alice/statuses/boost_dedup/activity",
            "published": "2024-06-10T09:00:00Z",
            "object": "https://other.social/users/bob/statuses/888"
        });

        let zip_path = make_outbox_zip_named("announce_dedup", &[create1, announce]);

        // First import.
        let out1 = (IMPORT.run)(&vault, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out1.counts.get("posts"), Some(&2), "first import: 1 create + 1 announce");
        assert_eq!(out1.counts.get("boosts"), Some(&1));
        assert_eq!(out1.counts.get("duplicates"), Some(&0));

        // Second import of the same ZIP — all items must be deduped.
        let out2 = (IMPORT.run)(&vault, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out2.counts.get("posts"), Some(&0), "re-import must not add new posts");
        assert_eq!(out2.counts.get("duplicates"), Some(&2), "both create + announce are duplicates");

        // Contract file has exactly 2 lines (1 post + 1 repost).
        let content =
            fs::read_to_string(vault.root().join("social/mastodon/2024-06.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 2, "exactly 1 post + 1 repost row, never duplicated");

        let _ = fs::remove_file(zip_path);
    }
}
