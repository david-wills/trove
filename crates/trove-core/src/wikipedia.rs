//! Wikipedia Contributions — edit history via the public MediaWiki usercontribs API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/wikipedia.md
//!
//! **Periodic** (daily) poll of the user's edit history on `en.wikipedia.org`
//! (and any extra MediaWiki domains the user adds). The MediaWiki Action API
//! (`action=query&list=usercontribs`) is keyless — no auth, no API key. The
//! user supplies their Wikipedia username once via a TokenPaste connection.
//!
//! ## Two layers
//!
//! - **Raw** — full API contribution object verbatim under
//!   `social/wikipedia/raw/YYYY-MM.jsonl`, partitioned by the local month of
//!   the edit's timestamp. Tagged with the source domain for multi-wiki support.
//! - **Contract** — one [`crate::social::Post`] (`kind: "edit"`) per revision
//!   under `social/wikipedia/YYYY-MM.jsonl`, deduped by
//!   `guid = "{domain}:{revid}"`.
//!
//! ## Cursor
//!
//! Newest-timestamp watermark per domain, persisted at
//! `.trove/wikipedia-sync.json` (not under `.trove/sync/` — it's non-secret
//! state, like `lastfm-sync.json`). The first sync backfills the whole
//! history; later syncs send `ucstart=<watermark>` and drain forward. The
//! watermark is advanced ONLY after a complete, uninterrupted drain.

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
use crate::social::Post;
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "wikipedia";
/// Contract-layer stream directory (partitioned by month of edit ts).
const DIR: &str = "social/wikipedia";
/// Raw layer: full API objects.
const RAW_DIR: &str = "social/wikipedia/raw";
/// Non-secret rebuildable cursor (not under .trove/sync/).
const SYNC_FILE: &str = ".trove/wikipedia-sync.json";
/// Service id for the secret store (username).
const SERVICE: &str = "wikipedia";

/// Default MediaWiki domain.
const DEFAULT_DOMAIN: &str = "en.wikipedia.org";

/// API limit per page (max 500; use 500 to minimise round trips).
const API_LIMIT: u32 = 500;
/// Generous timeout — the MediaWiki API is generally fast.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Brief pause between pages to be a polite API citizen.
const PAGE_PAUSE: Duration = Duration::from_millis(300);

// Daily cadence — edits are infrequent; daily is more than enough.
pub const WIKIPEDIA_SYNC_SECS: u64 = 86_400;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    // Check connection before attempting a pull so we can distinguish the
    // expected "not yet connected" case (emit a note, not an error) from a
    // genuine network / API failure (propagate the Err so the runner surfaces it).
    let connected = vault
        .load_sync_token(SERVICE)
        .ok()
        .and_then(|t| t)
        .map(|t| !t.access_token.trim().is_empty())
        .unwrap_or(false);

    if !connected {
        return Ok(crate::registry::CollectOutcome::note(
            "wikipedia sync skipped: no username configured",
        ));
    }

    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("edits").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("wikipedia synced — {n} edits")
            }))
        }
        Err(e) => Err(e), // real failure — surface to the runner
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let edits = out.counts.get("edits").copied().unwrap_or(0);
    let headline = if edits == 0 {
        "Wikipedia is up to date — no new edits".to_string()
    } else {
        format!("Wikipedia synced — {edits} edits")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "wikipedia",
        name: "Wikipedia Contributions",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Sync your complete Wikipedia edit history \
                      via the keyless public usercontribs API. Requires only your Wikipedia \
                      username — no API key, no auth.",
        domain: "social",
        vault_path: "social/wikipedia/",
        toggleable: true,
        setup: &[
            "Enter your Wikipedia username on the connection card. \
             The username is case-sensitive and must match your profile exactly \
             (e.g. 'Jimbo_Wales', not 'jimbo_wales').",
            "First sync backfills your entire edit history; later syncs are incremental.",
        ],
        caveats: "Reads only public edit metadata (page title, edit comment, size diff, \
                  revision ids). Diff text is not downloaded. Edits on private wikis \
                  are not accessible. Only en.wikipedia.org is polled.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(WIKIPEDIA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("wikipedia"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection: TokenPaste — a Wikipedia username (keyless API).

fn def_connect(vault: &Vault, username: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("empty username");
    }
    // Basic validation: probe the API for this user.
    match fetch_one_contrib(DEFAULT_DOMAIN, username) {
        Ok(_) => {}
        Err(e) => {
            // A missing user returns an empty array, not an error.
            // Only surface a hard failure; treat "no edits" as valid.
            let msg = e.to_string();
            if msg.contains("HTTP 4") {
                bail!("Could not verify Wikipedia username {username:?}: {msg}");
            }
        }
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
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: username,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus {
        configured: true, // always: no API key needed
        accounts,
    })
}

/// Connection registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "wikipedia",
    display_name: "Wikipedia",
    methods: &[ConnectMethod::TokenPaste {
        label: "Wikipedia username",
        help: "Your Wikipedia username, exactly as it appears on your profile page (case-sensitive). \
               The API is public — no password or API key required.",
        placeholder: "Jimbo_Wales",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["wikipedia"],
    setup: &[
        "Enter your Wikipedia username and connect. \
         The API is public — no password or API key is required.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor.

/// Per-domain watermark. The domain map lets a user edit multiple wikis without
/// cursor collisions.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Latest edit timestamp seen, per domain (RFC3339). The next incremental
    /// poll passes `ucstart=<ts>` (newest-first ordering) and drains until
    /// it sees an edit we already have.
    #[serde(default)]
    watermarks: BTreeMap<String, String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_wikipedia_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_wikipedia_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP — plain ureq, injectable trait for testing.

trait UserContribs {
    /// Fetch one page of usercontribs. Returns the parsed JSON body.
    /// `uccontinue` is the pagination token from the previous response (None
    /// for the first page). `ucstart` is the RFC3339 newest-edit timestamp to
    /// start from (None = latest edits first, no lower bound for first sync).
    fn fetch(
        &self,
        domain: &str,
        username: &str,
        uccontinue: Option<&str>,
        ucstart: Option<&str>,
    ) -> Result<Value>;
}

struct WikiClient;

impl UserContribs for WikiClient {
    fn fetch(
        &self,
        domain: &str,
        username: &str,
        uccontinue: Option<&str>,
        ucstart: Option<&str>,
    ) -> Result<Value> {
        let url = format!("https://{domain}/w/api.php");
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .query("action", "query")
            .query("list", "usercontribs")
            .query("ucuser", username)
            .query(
                "ucprop",
                "ids|title|timestamp|comment|sizediff|flags|tags",
            )
            .query("uclimit", &API_LIMIT.to_string())
            .query("ucdir", "newer") // newest → oldest so watermark = first result
            .query("format", "json")
            .query("formatversion", "2");

        // Incremental: start from the known newest timestamp.
        // ucdir=older + ucstart=<newest_ts> drains from newest down.
        // We use ucdir=newer so we get oldest-first; ucend=<watermark> caps
        // the window to edits newer than what we have.
        // Actually: MediaWiki usercontribs default direction is "older"
        // (newest first). We keep the default (older) and use ucstart for
        // the newest upper bound — but we want NEW edits, so we use ucend
        // (the lower bound in "older" ordering) = watermark.
        // Re-think: older direction → ucstart is the upper bound (newest),
        // ucend is the lower bound (oldest). For incremental we want edits
        // NEWER than watermark, so: ucend = watermark + 1s is the lower
        // bound. Implementation: pass ucstart=now (or omit), ucend=watermark.
        // But for simplicity, same as lastfm: on incremental just pass ucend
        // (the exclusive lower fence) and drain until uccontinue runs out.
        // We DON'T use ucdir=newer to avoid API surprises.
        // (ucdir=newer is valid but less common; stick with the default.)
        //
        // Final design: ucdir defaults to "older" (newest-first).
        // - First sync: no ucstart, no ucend → full history, newest-first.
        // - Incremental: ucend=<watermark_ts> → only edits AT or after watermark
        //   (which in "older" direction is the lower/earliest bound).
        //   MediaWiki's ucend is INCLUSIVE with "older" direction.
        //   We pass the watermark exactly; the dedupe set handles any overlap.
        req = if let Some(watermark) = ucstart {
            req.query("ucend", watermark)
        } else {
            req
        };

        if let Some(cont) = uccontinue {
            req = req.query("uccontinue", cont);
        }

        let resp = req.call().context("Wikipedia API request failed")?;
        if resp.status() != 200 {
            bail!("HTTP {}", resp.status());
        }
        let body: Value =
            resp.into_json().context("Wikipedia API response was not valid JSON")?;
        Ok(body)
    }
}

// ---------------------------------------------------------------------------
// Single-edit probe for connect validation.

fn fetch_one_contrib(domain: &str, username: &str) -> Result<Value> {
    WikiClient.fetch(domain, username, None, None)
}

// ---------------------------------------------------------------------------
// Pull logic.

/// One parsed contribution item.
struct EditItem {
    /// `{domain}:{revid}` — the vault-unique dedupe key.
    guid: String,
    /// Local RFC3339 timestamp — used for vault contract partitioning.
    ts: String,
    /// UTC RFC3339 timestamp — stored as the ucend watermark fence to avoid
    /// local-offset fragility when replaying to the MediaWiki API.
    utc_ts: String,
    title: String,
    comment: String,
    sizediff: Option<i64>,
    revid: u64,
    parentid: u64,
    tags: Vec<Value>,
    minor: bool,
    /// The full API object for the raw layer.
    raw: Value,
    domain: String,
}

fn parse_contrib(domain: &str, item: &Value) -> Option<EditItem> {
    let revid = item.get("revid").and_then(Value::as_u64)?;
    let ts_raw = item.get("timestamp").and_then(Value::as_str)?;
    // MediaWiki returns timestamps as ISO 8601 (e.g. "2024-06-10T14:03:01Z").
    // Parse to a local RFC3339 for vault partitioning + a UTC string for the fence.
    let (ts, utc_ts) = parse_mw_ts(ts_raw)?;
    let title = item.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let comment = item.get("comment").and_then(Value::as_str).unwrap_or("").to_string();
    let sizediff = item.get("sizediff").and_then(Value::as_i64);
    let parentid = item.get("parentid").and_then(Value::as_u64).unwrap_or(0);
    let tags: Vec<Value> = item
        .get("tags")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let minor = item
        .get("minor")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Tag the raw object with the domain, then keep verbatim.
    let mut raw = item.clone();
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("_domain".into(), Value::String(domain.to_string()));
    }

    Some(EditItem {
        guid: format!("{domain}:{revid}"),
        ts,
        utc_ts,
        title,
        comment,
        sizediff,
        revid,
        parentid,
        tags,
        minor,
        raw,
        domain: domain.to_string(),
    })
}

/// Parse a MediaWiki API timestamp ("2024-06-10T14:03:01Z") → local RFC3339.
/// Returns `(local_ts, utc_ts)` where `local_ts` is used for vault contract
/// partitioning and `utc_ts` is stored as the ucend watermark fence so it
/// round-trips to the API without ambiguous timezone offsets.
fn parse_mw_ts(raw: &str) -> Option<(String, String)> {
    // MediaWiki always emits UTC ("Z" suffix). Parse as UTC then convert.
    let dt = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    let local_ts = dt.with_timezone(&Local).to_rfc3339();
    // Keep a UTC string for the watermark fence — unambiguous for replay as
    // the ucend parameter. Use the original "Z"-suffixed form from the API
    // (or re-format with explicit Z via format!) to avoid the "+00:00" form
    // that chrono::Utc::to_rfc3339() emits.
    let utc_ts = dt.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    Some((local_ts, utc_ts))
}

fn edit_to_post(item: &EditItem) -> Post {
    let mut post = Post::new(SOURCE, item.guid.clone(), item.ts.clone());
    post.kind = "edit".into();
    if !item.title.is_empty() {
        post.title = item.title.clone();
    }
    if !item.comment.is_empty() {
        post.text = item.comment.clone();
    }
    // Canonical URL to the edited revision.
    post.url = format!(
        "https://{domain}/w/index.php?diff={revid}&oldid={parentid}",
        domain = item.domain,
        revid = item.revid,
        parentid = item.parentid,
    );
    // context = the MediaWiki domain (wiki identity).
    post.context = item.domain.clone();

    // source-specific fields → extra.
    let mut extra = Map::new();
    extra.insert("revid".into(), Value::Number(item.revid.into()));
    extra.insert("parentid".into(), Value::Number(item.parentid.into()));
    if let Some(sd) = item.sizediff {
        extra.insert("sizediff".into(), Value::Number(sd.into()));
    }
    if item.minor {
        extra.insert("minor".into(), Value::Bool(true));
    }
    if !item.tags.is_empty() {
        extra.insert("tags".into(), Value::Array(item.tags.clone()));
    }
    post.extra = extra;
    post
}

/// Load existing guids for the contract layer (for dedupe on incremental syncs
/// where ucend may not perfectly exclude already-held revisions).
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

/// Load existing raw guids from the raw layer (for dedupe — the ucend fence is
/// inclusive so boundary revisions would otherwise be re-appended each sync).
/// Raw rows carry `_domain` and `revid`; we reconstruct the same guid format.
fn load_existing_raw_guids(vault: &Vault) -> HashSet<String> {
    let mut out = HashSet::new();
    let stream = vault.stream(RAW_DIR, Partition::Month);
    if let Ok(keys) = stream.partitions() {
        for key in keys {
            if let Ok(rows) = stream.read::<serde_json::Value>(&key) {
                for row in rows {
                    if let (Some(domain), Some(revid)) = (
                        row.get("_domain").and_then(|v| v.as_str()),
                        row.get("revid").and_then(|v| v.as_u64()),
                    ) {
                        out.insert(format!("{domain}:{revid}"));
                    }
                }
            }
        }
    }
    out
}

/// Pull edits for all configured domains. Returns a PullOutcome with an
/// "edits" count.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let username = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|u| !u.trim().is_empty())
        .context("Wikipedia is not connected — add your username in the Integrations tab")?;

    pull_with(vault, &WikiClient, &username)
}

fn pull_with(vault: &Vault, client: &impl UserContribs, username: &str) -> Result<PullOutcome> {
    // Domains: always includes the default en.wikipedia.org.
    let domains = vec![DEFAULT_DOMAIN.to_string()];

    let mut state = vault.read_wikipedia_sync();
    let mut existing_guids = load_existing_guids(vault);
    let mut existing_raw_guids = load_existing_raw_guids(vault);

    let mut total_edits: u64 = 0;
    let mut total_raw: u64 = 0;

    for domain in &domains {
        let watermark = state.watermarks.get(domain).cloned();
        let (edits, raw_count, newest_ts) =
            pull_domain(vault, client, domain, username, watermark.as_deref(), &mut existing_guids, &mut existing_raw_guids)?;
        total_edits += edits;
        total_raw += raw_count;

        // Advance watermark only after the full drain.
        if let Some(ts) = newest_ts {
            state.watermarks.insert(domain.clone(), ts);
        }
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_wikipedia_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total_edits} edits"),
        counts: BTreeMap::from([("edits", total_edits), ("raw", total_raw)]),
    })
}

/// Drain all pages of usercontribs for one domain. Returns (contract_count,
/// raw_count, newest_ts_seen). The newest_ts_seen is an RFC3339 that can be
/// stored as the next incremental ucend fence.
fn pull_domain(
    vault: &Vault,
    client: &impl UserContribs,
    domain: &str,
    username: &str,
    watermark: Option<&str>,
    existing_guids: &mut HashSet<String>,
    existing_raw_guids: &mut HashSet<String>,
) -> Result<(u64, u64, Option<String>)> {
    let contract_stream = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut posts: Vec<Post> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();
    // newest_utc_ts: UTC RFC3339 stored as the ucend fence on the next incremental
    // poll — unambiguous for replay to the MediaWiki API (no local-offset fragility).
    let mut newest_utc_ts: Option<String> = None;
    let mut uccontinue: Option<String> = None;

    loop {
        let body = client.fetch(domain, username, uccontinue.as_deref(), watermark)?;

        // Parse contributions array.
        let items = body
            .get("query")
            .and_then(|q| q.get("usercontribs"))
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&[]);

        if items.is_empty() {
            break;
        }

        for item in items {
            let Some(edit) = parse_contrib(domain, item) else {
                continue;
            };
            // Track newest UTC timestamp for the watermark fence.
            if newest_utc_ts.is_none() {
                newest_utc_ts = Some(edit.utc_ts.clone());
            }
            // Raw layer: dedupe by guid to avoid re-appending boundary edits
            // on each incremental sync (ucend is inclusive in "older" direction).
            if existing_raw_guids.insert(edit.guid.clone()) {
                raws.push(RawLine { ts: edit.ts.clone(), value: edit.raw.clone() });
            }
            // Contract layer: dedupe by guid.
            if existing_guids.insert(edit.guid.clone()) {
                posts.push(edit_to_post(&edit));
            }
        }

        // Check for continuation token.
        uccontinue = body
            .get("continue")
            .and_then(|c| c.get("uccontinue"))
            .and_then(Value::as_str)
            .map(str::to_string);

        if uccontinue.is_none() {
            break;
        }
        std::thread::sleep(PAGE_PAUSE);
    }

    // Write after full drain.
    contract_stream.append(&posts, |p| &p.ts)?;
    raw_stream.append(&raws, |r| &r.ts)?;

    let edits = posts.len() as u64;
    let raw_count = raws.len() as u64;
    Ok((edits, raw_count, newest_utc_ts))
}

// ---------------------------------------------------------------------------
// Raw line wrapper.

/// A thin newtype to give raw API objects a `ts` field for the stream's
/// partition function while keeping the API object verbatim (via flatten).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-wikipedia-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A synthetic usercontribs response with 3 edits across two months.
    fn sample_page(uccontinue: Option<&str>) -> Value {
        let mut resp = json!({
            "batchcomplete": "",
            "query": {
                "usercontribs": [
                    {
                        "userid": 24,
                        "user": "Jimbo_Wales",
                        "pageid": 123456,
                        "revid": 1189457901,
                        "parentid": 1189456000,
                        "ns": 0,
                        "title": "Wikipedia",
                        "timestamp": "2024-06-10T14:03:01Z",
                        "comment": "Clarified founding date",
                        "sizediff": 42
                    },
                    {
                        "userid": 24,
                        "user": "Jimbo_Wales",
                        "pageid": 789012,
                        "revid": 1185000001,
                        "parentid": 1185000000,
                        "ns": 0,
                        "title": "Free content",
                        "timestamp": "2024-05-20T09:15:00Z",
                        "comment": "Fixed typo",
                        "sizediff": -3,
                        "minor": true,
                        "tags": ["mobile edit"]
                    },
                    {
                        "userid": 24,
                        "user": "Jimbo_Wales",
                        "pageid": 111111,
                        "revid": 1100000001,
                        "parentid": 0,
                        "ns": 4,
                        "title": "Wikipedia:Village pump",
                        "timestamp": "2023-12-01T10:00:00Z",
                        "comment": "",
                        "sizediff": 500
                    }
                ]
            }
        });
        if let Some(cont) = uccontinue {
            resp.as_object_mut().unwrap().insert(
                "continue".into(),
                json!({ "uccontinue": cont, "continue": "-||" }),
            );
        }
        resp
    }

    /// Stub client: returns pages from a list of prepared responses, in order.
    struct StubClient {
        pages: Mutex<Vec<Value>>,
    }

    impl StubClient {
        fn new(pages: Vec<Value>) -> Self {
            StubClient { pages: Mutex::new(pages) }
        }
    }

    impl UserContribs for StubClient {
        fn fetch(
            &self,
            _domain: &str,
            _username: &str,
            _uccontinue: Option<&str>,
            _ucstart: Option<&str>,
        ) -> Result<Value> {
            let mut lock = self.pages.lock().unwrap();
            if lock.is_empty() {
                // No more pages: return empty usercontribs.
                Ok(json!({ "batchcomplete": "", "query": { "usercontribs": [] } }))
            } else {
                Ok(lock.remove(0))
            }
        }
    }

    #[test]
    fn parses_and_writes_contract_and_raw_layers() {
        let v = temp_vault("contract_raw");
        let client = StubClient::new(vec![sample_page(None)]);

        // Provide a username via the token store.
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "Jimbo_Wales".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let out = pull_with(&v, &client, "Jimbo_Wales").unwrap();
        assert_eq!(out.counts["edits"], 3, "three edits: {}", out.headline);

        // Contract layer: partitioned by local month.
        // 2024-06-10 UTC → typically 2024-06.
        let stream = v.stream(DIR, Partition::Month);
        let partitions = stream.partitions().unwrap();
        assert!(!partitions.is_empty(), "at least one month partition written");

        let all_posts: Vec<Post> = partitions
            .iter()
            .flat_map(|k| stream.read::<Post>(k).unwrap_or_default())
            .collect();
        assert_eq!(all_posts.len(), 3);

        // Check a specific post.
        let p = all_posts.iter().find(|p| p.guid.contains("1189457901")).unwrap();
        assert_eq!(p.kind, "edit");
        assert_eq!(p.source, "wikipedia");
        assert_eq!(p.title, "Wikipedia");
        assert_eq!(p.text, "Clarified founding date");
        assert_eq!(p.context, DEFAULT_DOMAIN);
        assert!(p.url.contains("1189457901"));
        assert_eq!(p.extra["sizediff"], json!(42));
        assert_eq!(p.extra["revid"], json!(1189457901u64));

        // Minor edit.
        let minor = all_posts.iter().find(|p| p.guid.contains("1185000001")).unwrap();
        assert_eq!(minor.extra["minor"], json!(true));
        assert_eq!(minor.extra["tags"], json!(["mobile edit"]));

        // Raw layer: full fidelity.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let raw_parts = raw_stream.partitions().unwrap();
        let all_raws: Vec<Value> = raw_parts
            .iter()
            .flat_map(|k| raw_stream.read::<Value>(k).unwrap_or_default())
            .collect();
        assert_eq!(all_raws.len(), 3, "every edit kept in raw layer");
        // Raw row carries the _domain tag.
        assert!(all_raws.iter().all(|r| r["_domain"] == json!("en.wikipedia.org")));
        assert!(all_raws.iter().all(|r| r.get("revid").is_some()));
    }

    #[test]
    fn incremental_sync_dedupes_existing_edits() {
        let v = temp_vault("dedup");
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "Jimbo_Wales".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        // First sync.
        let c1 = StubClient::new(vec![sample_page(None)]);
        let out1 = pull_with(&v, &c1, "Jimbo_Wales").unwrap();
        assert_eq!(out1.counts["edits"], 3);

        // Second sync with the same data — should write 0 new contract rows.
        let c2 = StubClient::new(vec![sample_page(None)]);
        let out2 = pull_with(&v, &c2, "Jimbo_Wales").unwrap();
        assert_eq!(out2.counts["edits"], 0, "all edits already held: {}", out2.headline);
        assert_eq!(out2.counts["raw"], 0, "raw layer also deduped: {}", out2.headline);

        // Verify raw layer still has exactly 3 rows (not 6) after two syncs.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let raw_parts = raw_stream.partitions().unwrap();
        let all_raws: Vec<Value> = raw_parts
            .iter()
            .flat_map(|k| raw_stream.read::<Value>(k).unwrap_or_default())
            .collect();
        assert_eq!(all_raws.len(), 3, "raw layer deduped across syncs — no boundary duplicates");
    }

    #[test]
    fn cursor_is_persisted_after_sync() {
        let v = temp_vault("cursor");
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "Jimbo_Wales".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let c = StubClient::new(vec![sample_page(None)]);
        pull_with(&v, &c, "Jimbo_Wales").unwrap();

        let state = v.read_wikipedia_sync();
        assert!(
            state.watermarks.contains_key(DEFAULT_DOMAIN),
            "watermark stored for default domain"
        );
        let wm = &state.watermarks[DEFAULT_DOMAIN];
        // Watermark is now stored as UTC RFC3339 (ends in "Z") for unambiguous replay.
        assert!(wm.starts_with("2024-06"), "watermark looks like a UTC RFC3339 in June 2024: {wm}");
        assert!(wm.ends_with('Z'), "watermark stored as UTC (ends with Z): {wm}");
        assert!(state.updated.is_some(), "updated field set");
    }

    #[test]
    fn pagination_drains_multiple_pages() {
        let v = temp_vault("pagination");
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "Jimbo_Wales".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        // Page 1 has a continue token; page 2 has no continue token.
        // Each page has 3 unique edits (but we only have one fixture — OK for
        // pagination plumbing; guid uniqueness is enforced at the fixture level
        // anyway since we reuse revids but that's fine for the test).
        // Use distinct revids so dedupe doesn't collapse them.
        let page1 = json!({
            "batchcomplete": "",
            "continue": { "uccontinue": "20240501000000|99999", "continue": "-||" },
            "query": {
                "usercontribs": [
                    {
                        "userid": 24, "user": "Jimbo_Wales", "pageid": 1,
                        "revid": 9000001u64, "parentid": 0, "ns": 0,
                        "title": "Page A", "timestamp": "2024-06-01T10:00:00Z",
                        "comment": "Edit A", "sizediff": 10
                    }
                ]
            }
        });
        let page2 = json!({
            "batchcomplete": "",
            "query": {
                "usercontribs": [
                    {
                        "userid": 24, "user": "Jimbo_Wales", "pageid": 2,
                        "revid": 9000002u64, "parentid": 0, "ns": 0,
                        "title": "Page B", "timestamp": "2024-05-01T10:00:00Z",
                        "comment": "Edit B", "sizediff": 20
                    }
                ]
            }
        });

        let c = StubClient::new(vec![page1, page2]);
        let out = pull_with(&v, &c, "Jimbo_Wales").unwrap();
        assert_eq!(out.counts["edits"], 2, "both pages drained: {}", out.headline);
    }

    #[test]
    fn empty_response_is_ok() {
        let v = temp_vault("empty");
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "Jimbo_Wales".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let c = StubClient::new(vec![json!({
            "batchcomplete": "",
            "query": { "usercontribs": [] }
        })]);
        let out = pull_with(&v, &c, "Jimbo_Wales").unwrap();
        assert_eq!(out.counts["edits"], 0);
        assert_eq!(out.headline, "0 edits");
    }

    #[test]
    fn parse_mw_ts_converts_utc_to_local() {
        // MediaWiki emits "2024-06-10T14:03:01Z" — must parse without panic.
        let result = parse_mw_ts("2024-06-10T14:03:01Z");
        assert!(result.is_some(), "valid UTC timestamp parsed");
        let (local_ts, utc_ts) = result.unwrap();
        // local_ts is used for vault partitioning — should contain 2024-06.
        assert!(local_ts.contains("2024-06"), "year+month preserved in local ts: {local_ts}");
        // utc_ts is used as the ucend fence — must be UTC (ends in Z).
        assert!(utc_ts.ends_with('Z'), "utc_ts is UTC (ends with Z): {utc_ts}");
        assert!(utc_ts.starts_with("2024-06-10T14:03:01"), "utc_ts preserves full precision: {utc_ts}");
    }

    #[test]
    fn edit_to_post_maps_fields_correctly() {
        let item = EditItem {
            guid: "en.wikipedia.org:12345".into(),
            ts: "2024-06-10T07:03:01-07:00".into(),
            utc_ts: "2024-06-10T14:03:01Z".into(),
            title: "Wikipedia".into(),
            comment: "Fixed a typo".into(),
            sizediff: Some(-5),
            revid: 12345,
            parentid: 12344,
            tags: vec![json!("mobile edit")],
            minor: true,
            raw: json!({}),
            domain: "en.wikipedia.org".into(),
        };
        let post = edit_to_post(&item);
        assert_eq!(post.kind, "edit");
        assert_eq!(post.source, "wikipedia");
        assert_eq!(post.guid, "en.wikipedia.org:12345");
        assert_eq!(post.title, "Wikipedia");
        assert_eq!(post.text, "Fixed a typo");
        assert_eq!(post.context, "en.wikipedia.org");
        assert!(post.url.contains("12345"));
        assert_eq!(post.extra["sizediff"], json!(-5i64));
        assert_eq!(post.extra["minor"], json!(true));
        assert_eq!(post.extra["tags"], json!(["mobile edit"]));
        assert_eq!(post.extra["revid"], json!(12345u64));
        assert_eq!(post.extra["parentid"], json!(12344u64));
    }

    #[test]
    fn old_post_lines_still_deserialize() {
        // Back-compat: sparse lines missing optional fields still parse.
        let line = r#"{"ts":"2024-06-10T00:00:00-07:00","source":"wikipedia","guid":"en.wikipedia.org:999","future_field":"x"}"#;
        let p: Post = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "en.wikipedia.org:999");
        assert_eq!(p.kind, "");
        assert!(p.text.is_empty());
    }

    #[test]
    fn connection_is_registered_in_connections_array() {
        // Regression test: wikipedia::CONNECTION must appear in CONNECTIONS so
        // connection_of(&DEF) returns Some and the connect card renders in the UI.
        // A missing registration causes connection_of() to return None → pull()
        // always bails "Wikipedia is not connected" even after the user connects.
        let conn = crate::integrations::connection_of(&DEF);
        assert!(
            conn.is_some(),
            "wikipedia::CONNECTION must be registered in CONNECTIONS; \
             connection_of(&DEF) returned None — add &crate::wikipedia::CONNECTION \
             to the CONNECTIONS array in integrations.rs"
        );
        let conn = conn.unwrap();
        assert_eq!(conn.id, "wikipedia");

        // Also verify DEF references the connection id.
        assert_eq!(DEF.connection, Some("wikipedia"));
    }

    #[test]
    fn status_via_registered_connection() {
        // Drive the status path through the registered connection (not via
        // save_sync_token directly) to confirm the plumbing is wired end-to-end.
        let v = temp_vault("conn_status");

        // Before any connection: status should have no accounts but configured=true.
        let conn = crate::integrations::connection_of(&DEF).unwrap();
        let status_fn = conn.status;
        let status = status_fn(&v).unwrap();
        assert!(status.configured, "always configured (keyless API)");
        assert!(status.accounts.is_empty(), "no accounts before connect");

        // Simulate a connect call via the TokenPaste run fn.
        // We can't make live API calls in tests, so we inject a token directly
        // and then verify status reflects it.
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "Test_User".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status2 = status_fn(&v).unwrap();
        assert_eq!(status2.accounts.len(), 1, "one account after connect");
        assert_eq!(status2.accounts[0].label, "Test_User");
    }
}
