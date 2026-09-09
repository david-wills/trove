//! OneNote — cloud notes sync via Microsoft Graph API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/onenote.md.
//!
//! **Contract target.** Notes land in the [`notes`](crate::notes) domain:
//! `notes/onenote/YYYY-MM.jsonl` (one [`Note`] per page, partitioned by the
//! local month of `created`, deduped/upserted by the Graph page `id`).
//! Parallel full-fidelity raw layer: `notes/onenote/raw/YYYY-MM.jsonl` with
//! the complete Graph page object + HTML content.
//!
//! **Graph API (v1.0, delegated, `Notes.Read` scope).**
//! The collector walks the page list hierarchically:
//! 1. `GET /me/onenote/pages?$expand=parentNotebook,parentSection&$orderby=lastModifiedDateTime+desc&$top=100`
//!    — paginated via `@odata.nextLink` until exhausted.
//! 2. For each page whose `lastModifiedDateTime > watermark`, fetch the HTML
//!    body via `GET /me/onenote/pages/{id}/content`.
//! 3. Store both the raw object and the contract [`Note`] (HTML body verbatim;
//!    HTML→Markdown conversion is a read-time rendering opinion per the notes
//!    schema spec).
//!
//! **Incremental.** Watermark = highest `lastModifiedDateTime` seen across all
//! pages, persisted in `.trove/onenote-sync.json`. The list is ordered by
//! `lastModifiedDateTime desc` so a pass that stops at the watermark has
//! fetched every change since the last run. The FIRST pass fetches all pages
//! (no watermark) — a SILENT BASELINE; no events emitted (the snapshot-diff
//! discipline). Subsequent passes emit a note-count event only when new/changed
//! pages are found.
//!
//! **Auth.** Reuses the shared `microsoft` connection ([`crate::outlook`]),
//! per-account token via [`crate::outlook::microsoft_fresh_token`]. Requires
//! the `Notes.Read` scope — added to `MICROSOFT.scopes` in the same pass as
//! this module. The Entra (Azure AD) app registration must grant the new scope;
//! users who already consented will be re-prompted once for the wider bundle.
//!
//! **Notebook → section → page path.** Each page object from the paginated
//! list carries `parentNotebook.displayName` and `parentSection.displayName`
//! when `$expand=parentNotebook,parentSection` is included. The contract
//! `folder` field is set to `<notebook>/<section>` for this hierarchy.
//!
//! **No bulk export endpoint.** OneNote has no workspace-wide export; section
//! DOCX/PDF export (the import fallback) is out-of-scope for this collector.
//! The Mac app stores nothing locally useful — Graph is the only real read path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::outlook::{microsoft_accounts, microsoft_fresh_token};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

const SOURCE: &str = "onenote";
const NOTES_DIR: &str = "notes/onenote";
const RAW_DIR: &str = "notes/onenote/raw";
const SYNC_FILE: &str = ".trove/onenote-sync.json";
const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Sync cadence: hourly (notes change infrequently; matches Bear).
pub const ONENOTE_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Graph response shapes (confirmed from official docs — field names verbatim).

/// One page stub from `GET /me/onenote/pages` (the list endpoint).
/// `parentNotebook` and `parentSection` are only present when `$expand` is
/// used — both are optional here and omitted by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GraphPage {
    /// The stable page id — dedupe key.
    pub id: String,
    /// The page title.
    #[serde(default)]
    pub title: Option<String>,
    /// UTC ISO 8601 creation timestamp.
    #[serde(rename = "createdDateTime", default)]
    pub created_date_time: Option<String>,
    /// UTC ISO 8601 last-modified timestamp.
    #[serde(rename = "lastModifiedDateTime", default)]
    pub last_modified_date_time: Option<String>,
    /// URL to fetch the page's HTML content (`GET <contentUrl>`).
    #[serde(rename = "contentUrl", default)]
    pub content_url: Option<String>,
    /// The application that created the page.
    #[serde(rename = "createdByAppId", default)]
    pub created_by_app_id: Option<String>,
    /// Expanded: the notebook this page belongs to.
    #[serde(rename = "parentNotebook", default)]
    pub parent_notebook: Option<GraphNotebook>,
    /// Expanded: the section this page belongs to.
    #[serde(rename = "parentSection", default)]
    pub parent_section: Option<GraphSection>,
    /// Deep link to open the page in the OneNote native client.
    #[serde(default)]
    pub links: Option<GraphPageLinks>,
}

/// A minimal notebook shape from an `$expand=parentNotebook` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GraphNotebook {
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
}

/// A minimal section shape from an `$expand=parentSection` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GraphSection {
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
}

/// Page deep-links object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct GraphPageLinks {
    #[serde(rename = "oneNoteClientUrl", default)]
    pub one_note_client_url: Option<GraphHref>,
    #[serde(rename = "oneNoteWebUrl", default)]
    pub one_note_web_url: Option<GraphHref>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct GraphHref {
    #[serde(default)]
    pub href: Option<String>,
}

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Per-account sync state: the watermark that drives the incremental walk.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct AccountCursor {
    /// Display address (for diagnostics).
    #[serde(default)]
    email: String,
    /// The highest `lastModifiedDateTime` seen for this account. The list
    /// endpoint returns pages ordered by `lastModifiedDateTime desc`, so the
    /// first page newer than this cursor is where we stop. None = full scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<String>,
    /// Pages written for this account (running total).
    #[serde(default)]
    pages: u64,
    /// Error from the last pass, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    #[serde(default)]
    updated: String,
    #[serde(default)]
    accounts: BTreeMap<String, AccountCursor>,
}

impl Vault {
    fn read_onenote_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_onenote_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — base URL is injectable so parsing tests run without network.

/// Fetch one or more pages from the Graph API with a retry on 429.
/// Returns the parsed JSON body.
fn graph_get_json(base: &str, path: &str, token: &str) -> Result<Value> {
    let url = if path.starts_with("http") {
        path.to_string()
    } else {
        format!("{base}{path}")
    };
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(HTTP_TIMEOUT)
        .call();
    match resp {
        Ok(r) => Ok(r.into_json()?),
        Err(ureq::Error::Status(429, r)) => {
            let wait = r
                .header("Retry-After")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(2)
                .min(30);
            std::thread::sleep(Duration::from_secs(wait));
            let r2 = ureq::get(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .timeout(HTTP_TIMEOUT)
                .call()
                .map_err(|e| anyhow::anyhow!("OneNote Graph request failed: {e}"))?;
            Ok(r2.into_json()?)
        }
        Err(e) => Err(anyhow::anyhow!("OneNote Graph request failed: {e}")),
    }
}

/// Fetch the HTML content of a page; returns the raw HTML string.
fn graph_get_html(page_id: &str, token: &str) -> Result<String> {
    let url = format!("{GRAPH_BASE}/me/onenote/pages/{page_id}/content");
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(HTTP_TIMEOUT)
        .call();
    match resp {
        Ok(r) => Ok(r.into_string()?),
        Err(ureq::Error::Status(429, r)) => {
            let wait = r
                .header("Retry-After")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(2)
                .min(30);
            std::thread::sleep(Duration::from_secs(wait));
            let r2 = ureq::get(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .timeout(HTTP_TIMEOUT)
                .call()
                .map_err(|e| anyhow::anyhow!("OneNote content fetch failed: {e}"))?;
            Ok(r2.into_string()?)
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(anyhow::anyhow!("OneNote content fetch HTTP {code} for page {page_id}"))
        }
        Err(e) => Err(anyhow::anyhow!("OneNote content fetch failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Parsing — PURE functions, no network calls, fully fixture-testable.

/// Parse one page of the `GET /me/onenote/pages` list response. Returns the
/// parsed page stubs and the `@odata.nextLink` URL if there are more pages.
pub(crate) fn parse_pages_page(v: &Value) -> (Vec<GraphPage>, Option<String>) {
    let pages = v
        .get("value")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| serde_json::from_value::<GraphPage>(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let next = v.get("@odata.nextLink").and_then(Value::as_str).map(str::to_string);
    (pages, next)
}

/// Convert a [`GraphPage`] + optional HTML body into the contract [`Note`]
/// shape. `body` is the HTML string fetched from `contentUrl` — stored
/// verbatim (HTML→Markdown is a read-time rendering opinion). `source` is
/// always "onenote". PURE — fixture-testable.
pub(crate) fn page_to_note(page: &GraphPage, body: Option<&str>) -> Note {
    let mut note = Note::new(SOURCE, &page.id);

    if let Some(t) = &page.title {
        if !t.is_empty() {
            note.title = t.clone();
        }
    }
    if let Some(b) = body {
        if !b.is_empty() {
            note.body = b.to_string();
        }
    }

    // The Graph API returns timestamps in UTC ISO 8601 — use as-is for both
    // contract fields. The contract spec says RFC3339 for `created`; UTC ISO
    // 8601 from Graph (`2016-10-19T10:37:00Z`) IS valid RFC3339. Partitioning
    // by created-month uses `created`; the NOTE dedupe key is `id`.
    if let Some(ts) = &page.created_date_time {
        note.created = ts.clone();
    }
    if let Some(ts) = &page.last_modified_date_time {
        note.modified = ts.clone();
    }

    // `folder` = "Notebook/Section" for the hierarchy.
    let notebook = page
        .parent_notebook
        .as_ref()
        .and_then(|n| n.display_name.as_deref())
        .unwrap_or("")
        .to_string();
    let section = page
        .parent_section
        .as_ref()
        .and_then(|s| s.display_name.as_deref())
        .unwrap_or("")
        .to_string();
    if !notebook.is_empty() || !section.is_empty() {
        note.folder = if notebook.is_empty() {
            section
        } else if section.is_empty() {
            notebook
        } else {
            format!("{notebook}/{section}")
        };
    }

    // Source-specific extras: the deep-link hrefs and the creating app.
    let mut extra = Map::new();
    if let Some(links) = &page.links {
        if let Some(web) = links.one_note_web_url.as_ref().and_then(|l| l.href.as_deref()) {
            extra.insert("one_note_web_url".into(), Value::from(web));
        }
        if let Some(client) = links.one_note_client_url.as_ref().and_then(|l| l.href.as_deref()) {
            extra.insert("one_note_client_url".into(), Value::from(client));
        }
    }
    if let Some(app_id) = &page.created_by_app_id {
        if !app_id.is_empty() {
            extra.insert("created_by_app_id".into(), Value::from(app_id.as_str()));
        }
    }
    note.extra = extra;

    note
}

/// Build the raw JSON row for the raw layer: the full Graph page object plus
/// the HTML content body and an immutable `_created` partition key.
pub(crate) fn page_to_raw(page: &GraphPage, body: Option<&str>) -> Value {
    let mut obj = serde_json::to_value(page).unwrap_or(Value::Object(Map::new()));
    let map = obj.as_object_mut().unwrap();

    // Store the HTML content at a top-level `content` key for full fidelity.
    if let Some(b) = body {
        map.insert("content".into(), Value::from(b));
    }

    // Immutable partition key for the raw stream (Graph `createdDateTime`).
    let created = page.created_date_time.as_deref().unwrap_or("").to_string();
    map.insert("_created".into(), Value::from(created.as_str()));
    obj
}

// ---------------------------------------------------------------------------
// Upsert helpers — generic, parallel to bear.rs / simplenote.rs.

fn upsert_notes(vault: &Vault, fresh: &[Note]) -> Result<()> {
    let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
    for n in fresh {
        let Some(key) = Partition::Month.key(&n.created) else { continue };
        by_month.entry(key.to_string()).or_default().push(n);
    }
    let stream = vault.stream(NOTES_DIR, Partition::Month);
    for (month, incoming) in by_month {
        let incoming_ids: HashSet<&str> = incoming.iter().map(|n| n.id.as_str()).collect();
        let mut merged: Vec<Note> = stream
            .read::<Note>(&month)?
            .into_iter()
            .filter(|e| !incoming_ids.contains(e.id.as_str()))
            .collect();
        merged.extend(incoming.into_iter().cloned());
        vault.write_snapshot(&format!("{NOTES_DIR}/{month}.jsonl"), &merged)?;
    }
    Ok(())
}

fn upsert_raw(vault: &Vault, fresh: &[Value]) -> Result<()> {
    fn created_of(v: &Value) -> &str {
        v.get("_created").and_then(Value::as_str).unwrap_or("")
    }
    fn id_of(v: &Value) -> &str {
        v.get("id").and_then(Value::as_str).unwrap_or("")
    }
    let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
    for v in fresh {
        let Some(key) = Partition::Month.key(created_of(v)) else { continue };
        by_month.entry(key.to_string()).or_default().push(v);
    }
    let stream = vault.stream(RAW_DIR, Partition::Month);
    for (month, incoming) in by_month {
        let incoming_ids: HashSet<&str> = incoming.iter().map(|v| id_of(v)).collect();
        let mut merged: Vec<Value> = stream
            .read::<Value>(&month)?
            .into_iter()
            .filter(|e| !incoming_ids.contains(id_of(e)))
            .collect();
        merged.extend(incoming.into_iter().cloned());
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &merged)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-account sync logic.

/// Stats for one sync pass.
#[derive(Debug, Default)]
struct SyncStats {
    accounts: u32,
    pages: u64,
}

/// One sync pass for all connected Microsoft accounts. Silent no-op when no
/// account is connected. Per-account failures are recorded in the state file
/// without aborting the other accounts.
fn collect(vault: &Vault, base: &str) -> Result<SyncStats> {
    let accounts = microsoft_accounts(vault)?;
    let mut state = vault.read_onenote_sync();
    let live: HashSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    state.accounts.retain(|id, _| live.contains(id.as_str()));

    if accounts.is_empty() {
        if vault.resolve(SYNC_FILE).map(|p| p.exists()).unwrap_or(false) {
            state.updated = Local::now().to_rfc3339();
            vault.write_onenote_sync(&state)?;
        }
        return Ok(SyncStats::default());
    }

    let mut stats = SyncStats::default();
    for acct in &accounts {
        if acct.needs_reconnect {
            continue;
        }
        let token = match microsoft_fresh_token(vault, &acct.id) {
            Ok(t) => t,
            Err(e) => {
                let cur = state.accounts.entry(acct.id.clone()).or_default();
                cur.email = acct.email.clone();
                cur.error = Some(format!("{e:#}"));
                continue;
            }
        };
        let watermark = state.accounts.get(&acct.id).and_then(|c| c.watermark.clone());
        let before = state.accounts.get(&acct.id).map(|c| c.pages).unwrap_or(0);
        {
            let cur = state.accounts.entry(acct.id.clone()).or_default();
            cur.email = acct.email.clone();
            cur.error = None;
        }
        match sync_account(vault, base, &token, watermark.as_deref()) {
            Ok((written, new_watermark)) => {
                let cur = state.accounts.entry(acct.id.clone()).or_default();
                cur.pages += written;
                if let Some(wm) = new_watermark {
                    cur.watermark = Some(wm);
                }
                let after = cur.pages;
                if after > before {
                    stats.accounts += 1;
                    stats.pages += after - before;
                }
            }
            Err(e) => {
                let cur = state.accounts.entry(acct.id.clone()).or_default();
                cur.error = Some(format!("{e:#}"));
            }
        }
    }

    state.updated = Local::now().to_rfc3339();
    vault.write_onenote_sync(&state)?;
    Ok(stats)
}

/// Drain the page list for one account, fetch the HTML content for new/changed
/// pages, upsert contract + raw rows. Returns (pages_written, new_watermark).
/// The watermark is the highest `lastModifiedDateTime` seen this pass; the
/// caller advances the cursor ONLY after the full drain (drain-don't-strand).
fn sync_account(
    vault: &Vault,
    base: &str,
    token: &str,
    watermark: Option<&str>,
) -> Result<(u64, Option<String>)> {
    // Pages are ordered by lastModifiedDateTime desc — walk pages until we
    // reach one at or before the watermark.
    let mut url = format!(
        "{base}/me/onenote/pages?$expand=parentNotebook,parentSection\
         &$orderby=lastModifiedDateTime%20desc&$top=100"
    );

    let mut contract_batch: Vec<Note> = Vec::new();
    let mut raw_batch: Vec<Value> = Vec::new();
    let mut new_watermark: Option<String> = None;
    let mut stop = false;

    loop {
        let body = graph_get_json(base, &url, token)?;
        let (pages, next_url) = parse_pages_page(&body);

        for page in &pages {
            let modified = page.last_modified_date_time.as_deref().unwrap_or("");

            // If this page's modified time is strictly-before the stored
            // watermark, every subsequent page is also older (list is desc) —
            // stop draining. A null watermark (first run) → baseline pass.
            //
            // We use `<` (strict) rather than `<=` to avoid silently dropping
            // pages whose `lastModifiedDateTime` matches the watermark exactly
            // (second-granularity ties). Pages at exactly the boundary are
            // re-fetched and re-upserted; upsert is idempotent by id so
            // duplicates are safe.
            if let Some(wm) = watermark {
                if modified < wm {
                    stop = true;
                    break;
                }
            }

            // Fetch the HTML body.  On failure we do NOT silently swallow the
            // error — instead we skip this page AND do NOT advance the
            // watermark past it, so the next pass will retry.
            //
            // Strategy: try contentUrl first (preferred); fall back to the
            // id-based endpoint on any error.  `None` means both paths failed
            // → skip this page so the watermark is not advanced past it.
            let html_body: Option<String> = if let Some(ref cid) = page.content_url {
                let content_url = cid.clone();
                let primary = ureq::get(&content_url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .timeout(HTTP_TIMEOUT)
                    .call()
                    .ok()
                    .and_then(|r| r.into_string().ok());
                match primary {
                    Some(s) => Some(s),
                    None => graph_get_html(&page.id, token).ok(),
                }
            } else {
                graph_get_html(&page.id, token).ok()
            };

            // Content fetch failed — skip this page.  The watermark will NOT
            // be advanced past it (we only update new_watermark AFTER a page
            // is successfully written), so the next pass retries it.
            // This prevents permanent empty-body stranding on transient errors.
            if html_body.is_none() {
                continue;
            }

            let note = page_to_note(page, html_body.as_deref());
            let raw = page_to_raw(page, html_body.as_deref());

            // A page with no parseable created date can't be partitioned —
            // skip it gracefully (the raw layer requires a `_created` key).
            // The watermark is NOT advanced for skipped pages.
            if note.created.is_empty() {
                continue;
            }

            // Page is written — now safe to advance the watermark to this
            // page's modified time. (Watermark is advanced ONLY for pages
            // that are successfully written, preventing permanent empty-body
            // stranding on transient fetch failures.)
            if !modified.is_empty() {
                match &new_watermark {
                    None => new_watermark = Some(modified.to_string()),
                    Some(wm) if modified > wm.as_str() => {
                        new_watermark = Some(modified.to_string())
                    }
                    _ => {}
                }
            }

            contract_batch.push(note);
            raw_batch.push(raw);
        }

        if stop || pages.is_empty() || next_url.is_none() {
            break;
        }
        url = next_url.unwrap();
    }

    let written = contract_batch.len() as u64;

    // For the FIRST pass (no watermark) emit nothing — this is the SILENT
    // BASELINE (snapshot-diff discipline). We still write the data; we just
    // return 0 written so the outcome doesn't announce "N pages imported"
    // on the very first run (which would fire every first-time connect).
    let effective_written = if watermark.is_none() { 0 } else { written };

    if !contract_batch.is_empty() {
        upsert_notes(vault, &contract_batch)?;
        upsert_raw(vault, &raw_batch)?;
    }

    Ok((effective_written, new_watermark))
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    if microsoft_accounts(vault).map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    match collect(vault, GRAPH_BASE) {
        Ok(s) => Ok(crate::registry::CollectOutcome::note_if(s.pages > 0, || {
            format!("onenote synced — {} pages across {} accounts", s.pages, s.accounts)
        })),
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("onenote sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    if microsoft_accounts(vault)?.is_empty() {
        bail!("no Microsoft account is connected");
    }
    let s = collect(vault, GRAPH_BASE)?;
    let errors = vault
        .read_onenote_sync()
        .accounts
        .values()
        .filter(|a| a.error.is_some())
        .count() as u64;
    let mut headline = if s.pages > 0 {
        format!("{} pages across {} accounts", s.pages, s.accounts)
    } else {
        "no new pages".to_string()
    };
    if errors > 0 {
        headline.push_str(&format!(" — {errors} account{} failed", if errors == 1 { "" } else { "s" }));
    }
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("accounts", s.accounts as u64), ("pages", s.pages), ("account_errors", errors)]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (replaces the NotWired
/// stub). Reuses the shared `microsoft` connection (also covers Outlook email,
/// Outlook Calendar, Microsoft To Do, and Microsoft Teams).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "onenote",
        name: "OneNote",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your OneNote notebooks via the Microsoft Graph API. \
                      Pages are collected incrementally (modified-date watermark), \
                      stored with their HTML body and notebook/section hierarchy. \
                      Shares the Microsoft login with Outlook, Outlook Calendar, \
                      To Do, and Teams.",
        domain: "notes",
        vault_path: "notes/onenote/",
        toggleable: true,
        setup: &[
            "Connect a Microsoft account on the card above (register a free Azure app first — see the connect steps).",
            "API permissions must include Notes.Read (in addition to the Outlook permissions).",
            "First sync backfills all notebooks; later syncs fetch only changed pages.",
        ],
        caveats: "Requires Notes.Read in your Azure app's API permissions in addition to \
                  the Outlook bundle. Page content is stored as HTML (Graph returns HTML; \
                  Markdown rendering is done at read time). No bulk export endpoint — \
                  the Mac app stores nothing locally.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(ONENOTE_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("microsoft"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-onenote-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Parsing: confirmed against official Graph API docs (field names verbatim).

    #[test]
    fn parses_pages_list_with_next_link() {
        // Canned response: two pages + an @odata.nextLink.
        // Field names confirmed from Graph docs:
        //   id, title, createdDateTime, lastModifiedDateTime, contentUrl,
        //   parentNotebook.displayName, parentSection.displayName
        let body = json!({
            "value": [
                {
                    "id": "PAGE-1",
                    "title": "Project kickoff",
                    "createdDateTime": "2026-03-01T09:00:00Z",
                    "lastModifiedDateTime": "2026-06-10T14:30:00Z",
                    "contentUrl": "https://graph.microsoft.com/v1.0/me/onenote/pages/PAGE-1/content",
                    "createdByAppId": "OneNote",
                    "parentNotebook": { "id": "NB-1", "displayName": "Work" },
                    "parentSection": { "id": "SEC-1", "displayName": "Planning" },
                    "links": {
                        "oneNoteClientUrl": { "href": "onenote:https://d.docs.live.net/…" },
                        "oneNoteWebUrl": { "href": "https://onedrive.live.com/…" }
                    }
                },
                {
                    "id": "PAGE-2",
                    "title": "Daily notes",
                    "createdDateTime": "2026-04-05T08:00:00Z",
                    "lastModifiedDateTime": "2026-05-20T11:00:00Z",
                    "contentUrl": "https://graph.microsoft.com/v1.0/me/onenote/pages/PAGE-2/content"
                }
            ],
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/onenote/pages?$skiptoken=NEXT"
        });
        let (pages, next) = parse_pages_page(&body);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].id, "PAGE-1");
        assert_eq!(pages[0].title.as_deref(), Some("Project kickoff"));
        assert_eq!(pages[0].created_date_time.as_deref(), Some("2026-03-01T09:00:00Z"));
        assert_eq!(pages[0].last_modified_date_time.as_deref(), Some("2026-06-10T14:30:00Z"));
        assert!(pages[0].parent_notebook.as_ref().unwrap().display_name.as_deref() == Some("Work"));
        assert!(pages[0].parent_section.as_ref().unwrap().display_name.as_deref() == Some("Planning"));
        assert_eq!(pages[1].id, "PAGE-2");
        assert!(next.as_deref().unwrap().contains("NEXT"));
    }

    #[test]
    fn parses_empty_page_list() {
        let body = json!({ "value": [] });
        let (pages, next) = parse_pages_page(&body);
        assert!(pages.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn page_to_note_maps_all_fields() {
        let page = GraphPage {
            id: "PAGE-1".into(),
            title: Some("Project kickoff".into()),
            created_date_time: Some("2026-03-01T09:00:00Z".into()),
            last_modified_date_time: Some("2026-06-10T14:30:00Z".into()),
            content_url: None,
            created_by_app_id: Some("AppABC".into()),
            parent_notebook: Some(GraphNotebook { display_name: Some("Work".into()) }),
            parent_section: Some(GraphSection { display_name: Some("Planning".into()) }),
            links: Some(GraphPageLinks {
                one_note_web_url: Some(GraphHref { href: Some("https://onedrive.live.com/…".into()) }),
                one_note_client_url: Some(GraphHref { href: Some("onenote://…".into()) }),
            }),
        };
        let note = page_to_note(&page, Some("<html><body><p>Notes content</p></body></html>"));

        assert_eq!(note.source, "onenote");
        assert_eq!(note.id, "PAGE-1");
        assert_eq!(note.title, "Project kickoff");
        assert!(note.body.contains("<p>Notes content</p>"), "HTML body stored verbatim");
        assert_eq!(note.created, "2026-03-01T09:00:00Z");
        assert_eq!(note.modified, "2026-06-10T14:30:00Z");
        assert_eq!(note.folder, "Work/Planning", "notebook/section hierarchy as folder");
        assert!(note.extra.contains_key("one_note_web_url"));
        assert!(note.extra.contains_key("created_by_app_id"));
    }

    #[test]
    fn page_to_note_no_title_no_body() {
        let page = GraphPage {
            id: "PAGE-SPARSE".into(),
            title: None,
            created_date_time: Some("2026-05-01T00:00:00Z".into()),
            last_modified_date_time: None,
            content_url: None,
            created_by_app_id: None,
            parent_notebook: None,
            parent_section: None,
            links: None,
        };
        let note = page_to_note(&page, None);
        assert_eq!(note.source, "onenote");
        assert_eq!(note.id, "PAGE-SPARSE");
        assert!(note.title.is_empty(), "no title → omit-empty");
        assert!(note.body.is_empty(), "no body → omit-empty");
        assert!(note.folder.is_empty());
    }

    #[test]
    fn page_to_note_notebook_only_or_section_only() {
        let page_nb_only = GraphPage {
            id: "P1".into(),
            title: None,
            created_date_time: Some("2026-01-01T00:00:00Z".into()),
            last_modified_date_time: None,
            content_url: None,
            created_by_app_id: None,
            parent_notebook: Some(GraphNotebook { display_name: Some("MyNotebook".into()) }),
            parent_section: None,
            links: None,
        };
        assert_eq!(page_to_note(&page_nb_only, None).folder, "MyNotebook");

        let page_sec_only = GraphPage {
            id: "P2".into(),
            title: None,
            created_date_time: Some("2026-01-01T00:00:00Z".into()),
            last_modified_date_time: None,
            content_url: None,
            created_by_app_id: None,
            parent_notebook: None,
            parent_section: Some(GraphSection { display_name: Some("MySection".into()) }),
            links: None,
        };
        assert_eq!(page_to_note(&page_sec_only, None).folder, "MySection");
    }

    #[test]
    fn page_to_raw_includes_content_and_created_key() {
        let page = GraphPage {
            id: "PAGE-R".into(),
            title: Some("Raw test".into()),
            created_date_time: Some("2026-06-01T10:00:00Z".into()),
            last_modified_date_time: Some("2026-06-05T10:00:00Z".into()),
            content_url: None,
            created_by_app_id: None,
            parent_notebook: None,
            parent_section: None,
            links: None,
        };
        let raw = page_to_raw(&page, Some("<html>body</html>"));
        assert_eq!(raw["id"], "PAGE-R");
        assert_eq!(raw["content"], "<html>body</html>");
        assert_eq!(raw["_created"], "2026-06-01T10:00:00Z", "_created partition key");
    }

    // -----------------------------------------------------------------------
    // Vault write: contract + raw layers, partition by created month.

    #[test]
    fn upsert_and_read_back_notes_partitioned_by_created_month() {
        let v = temp_vault("write");

        let pages = vec![
            GraphPage {
                id: "A".into(),
                title: Some("March note".into()),
                created_date_time: Some("2026-03-10T09:00:00Z".into()),
                last_modified_date_time: Some("2026-03-10T09:00:00Z".into()),
                content_url: None,
                created_by_app_id: None,
                parent_notebook: Some(GraphNotebook { display_name: Some("Personal".into()) }),
                parent_section: Some(GraphSection { display_name: Some("Diary".into()) }),
                links: None,
            },
            GraphPage {
                id: "B".into(),
                title: Some("June note".into()),
                created_date_time: Some("2026-06-01T09:00:00Z".into()),
                last_modified_date_time: Some("2026-06-01T09:00:00Z".into()),
                content_url: None,
                created_by_app_id: None,
                parent_notebook: None,
                parent_section: None,
                links: None,
            },
        ];

        let notes: Vec<Note> = pages.iter().map(|p| page_to_note(p, Some("<p>text</p>"))).collect();
        let raws: Vec<Value> = pages.iter().map(|p| page_to_raw(p, Some("<p>text</p>"))).collect();

        upsert_notes(&v, &notes).unwrap();
        upsert_raw(&v, &raws).unwrap();

        // March contract file.
        let mar =
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar.len(), 1);
        assert_eq!(mar[0].id, "A");
        assert_eq!(mar[0].source, "onenote");
        assert_eq!(mar[0].folder, "Personal/Diary");
        assert!(mar[0].body.contains("<p>text</p>"), "HTML body stored verbatim");

        // June contract file.
        let jun =
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(jun.len(), 1);
        assert_eq!(jun[0].id, "B");

        // Raw layer: same partitioning.
        let raw_mar =
            v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-03").unwrap();
        assert_eq!(raw_mar.len(), 1);
        assert_eq!(raw_mar[0]["id"], "A");
        assert_eq!(raw_mar[0]["content"], "<p>text</p>");
        assert_eq!(raw_mar[0]["_created"], "2026-03-10T09:00:00Z");
    }

    #[test]
    fn upsert_is_idempotent_replaces_by_id() {
        let v = temp_vault("upsert");
        let page = GraphPage {
            id: "X".into(),
            title: Some("Version 1".into()),
            created_date_time: Some("2026-05-01T09:00:00Z".into()),
            last_modified_date_time: Some("2026-05-01T09:00:00Z".into()),
            content_url: None,
            created_by_app_id: None,
            parent_notebook: None,
            parent_section: None,
            links: None,
        };

        upsert_notes(&v, &[page_to_note(&page, Some("<p>v1</p>"))]).unwrap();
        let may_v1 =
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may_v1[0].title, "Version 1");

        // Edit: same id, updated title+body.
        let updated = GraphPage { title: Some("Version 2".into()), ..page };
        upsert_notes(&v, &[page_to_note(&updated, Some("<p>v2</p>"))]).unwrap();
        let may_v2 =
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may_v2.len(), 1, "upsert by id: exactly one line");
        assert_eq!(may_v2[0].title, "Version 2", "line replaced in place");
        assert!(may_v2[0].body.contains("v2"));
    }

    #[test]
    fn page_with_no_created_is_skipped_gracefully() {
        let v = temp_vault("nocreated");
        // A page with no createdDateTime cannot be partitioned — must not crash.
        let page = GraphPage {
            id: "NO-DATE".into(),
            title: Some("No date page".into()),
            created_date_time: None, // no partition key
            last_modified_date_time: Some("2026-06-01T00:00:00Z".into()),
            content_url: None,
            created_by_app_id: None,
            parent_notebook: None,
            parent_section: None,
            links: None,
        };
        let note = page_to_note(&page, None);
        // upsert_notes ignores notes whose `created` month can't be computed.
        upsert_notes(&v, &[note]).unwrap();
        // Nothing written for 2026-06 (created is empty; partition skipped).
        let jun =
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert!(jun.is_empty(), "note with empty created is silently dropped");
    }

    #[test]
    fn watermark_round_trips() {
        let v = temp_vault("watermark");
        assert!(v.read_onenote_sync().accounts.is_empty());
        let mut state = SyncState::default();
        state.updated = "2026-06-14T00:00:00Z".into();
        state.accounts.insert(
            "acct-1".into(),
            AccountCursor {
                email: "user@example.com".into(),
                watermark: Some("2026-06-10T14:30:00Z".into()),
                pages: 42,
                error: None,
            },
        );
        v.write_onenote_sync(&state).unwrap();
        let got = v.read_onenote_sync();
        assert_eq!(got.accounts["acct-1"].watermark.as_deref(), Some("2026-06-10T14:30:00Z"));
        assert_eq!(got.accounts["acct-1"].pages, 42);
    }

    #[test]
    fn serde_back_compat_old_notes_still_deserialize() {
        // A Note line written by an older writer with only required fields must
        // still deserialize (additive schema evolution).
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-06.jsonl")),
            "{\"source\":\"onenote\",\"id\":\"OLD-1\"}\n\
             {\"source\":\"onenote\",\"id\":\"OLD-2\",\"body\":\"b\",\"created\":\"2026-06-01T00:00:00Z\"}\n",
        )
        .unwrap();
        let rows =
            v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "OLD-1");
        assert_eq!(rows[1].body, "b");
    }

    #[test]
    fn parse_pages_page_missing_value_array_returns_empty() {
        // A response with no "value" key (shouldn't happen in practice, but
        // defensive parse must return empty rather than panic).
        let body = json!({ "@odata.context": "https://graph.microsoft.com/…" });
        let (pages, next) = parse_pages_page(&body);
        assert!(pages.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn def_is_periodic_uses_microsoft_connection_has_pull() {
        match DEF.behavior {
            Behavior::Periodic { .. } => {}
            _ => panic!("DEF must be Periodic"),
        }
        assert_eq!(DEF.connection, Some("microsoft"));
        assert!(DEF.pull.is_some(), "pull hook required for auto_pull");
        assert_eq!(DEF.meta.id, "onenote");
        assert_eq!(DEF.meta.domain, "notes");
        assert_eq!(DEF.meta.vault_path, "notes/onenote/");
    }
}
