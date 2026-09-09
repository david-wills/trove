//! Hypothesis — web annotation service with a clean public REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/hypothesis.md
//!
//! A **Periodic** cloud pull over a single endpoint:
//!
//! - `GET https://api.hypothes.is/api/search?user=acct:USERNAME@hypothes.is`
//!   with `Authorization: Bearer TOKEN`. Each annotation becomes a
//!   [`crate::reading::Highlight`] under
//!   `reading/hypothesis/highlights/YYYY-MM.jsonl` (`guid` = annotation `id`;
//!   `ts` = `created` (annotation make-time, not edit-time); `text` = highlighted
//!   passage from `target[0].selector[TextQuoteSelector].exact`; `note` =
//!   annotation `text` body; `title` from `document.title[0]`; `url` =
//!   annotation `uri`; `tags` from `tags[]`; `group`, `links.incontext`, and
//!   `updated` into `extra`).
//!
//! Two layers per annotation: the **raw** API object verbatim under
//! `reading/hypothesis/raw/YYYY-MM.jsonl` (full fidelity, unconditional), and
//! the normalized **contract** rows under
//! `reading/hypothesis/highlights/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! The search endpoint paginates via `sort=id&order=asc&search_after=<last_id>`.
//! Using annotation `id` (unique per row) as the pagination cursor guarantees
//! forward progress even when many annotations share the same `updated`
//! timestamp (e.g. bulk imports or group migrations). The watermark stores the
//! max `updated` across the drain and the last `id` seen; the drain is only
//! started from the last `id` cursor, not from an `updated` timestamp (which
//! has the timestamp-tie problem with strict-`>` semantics). The watermark
//! advances only **after** the full drain so a crash re-drains without loss.
//!
//! The token is stored in `.trove/sync/hypothesis` (0600) via
//! [`Vault::save_sync_token`]. The username (needed for `acct:USERNAME@…`) is
//! a non-secret stored in `.trove/hypothesis-sync.json`. They are captured as
//! two separate fields in the [`ConnectMethod::TokenPaste`] composite: the
//! pasted string is `USERNAME::TOKEN` (the run function splits on `::`).
//!
//! ## API evidence
//!
//! - `GET /api/search` query parameters: `user`, `sort`, `order`, `limit`,
//!   `search_after` (ISO8601 cursor for the `sort` field).
//! - Response: `{"total": N, "rows": [...]}` — `total` is unreliable (capped at
//!   10 000); stop pagination on an empty or short page.
//! - Annotation fields confirmed via hypexport dal.py + live API:
//!   `id`, `created`, `updated`, `uri`, `text`, `tags` (string[]),
//!   `group`, `user`, `target[0].selector[N].exact` (TextQuoteSelector),
//!   `document.title` (string[]), `links.incontext`.
//!
//! `ts` is mapped from `created` (the time the annotation was originally
//! made), NOT from `updated` (edit time). `updated` is kept only as the
//! watermark/cursor field so edits are re-fetched. This matches readwise.rs
//! which uses `highlighted_at` (make-time) for `ts` and `updated`/`updatedAfter`
//! only as the cursor.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::reading::Highlight;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Highlights live under `highlights/`, raw under `raw/`.
const HIGHLIGHTS_DIR: &str = "reading/hypothesis/highlights";
const RAW_DIR: &str = "reading/hypothesis/raw";

/// Non-secret rebuildable cursor (username + watermark). NOT under `.trove/sync/`.
const SYNC_FILE: &str = ".trove/hypothesis-sync.json";

/// Service id under `.trove/sync/` where the Bearer token is stored (0600).
const SERVICE: &str = "hypothesis";

const API_BASE: &str = "https://api.hypothes.is";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Page size — well within the API's documented default/max.
const PAGE_LIMIT: usize = 200;
/// Seconds between syncs: hourly, matching readwise (annotations trickle in).
pub const HYPOTHESIS_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(HIGHLIGHTS_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let n = out.counts.get("highlights").copied().unwrap_or(0);
                format!("hypothesis synced — {n} annotations")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("hypothesis sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("highlights").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Hypothesis synced — {n} annotations"),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "hypothesis",
        name: "Hypothesis",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your public and private web annotations from Hypothesis, \
                      including highlights, notes, and group annotations.",
        domain: "reading",
        vault_path: "reading/hypothesis/",
        toggleable: true,
        setup: &[
            "Connect with your Hypothesis username and API token on this card.",
            "First sync backfills all annotations; later syncs are incremental.",
        ],
        caveats: "Annotations in private groups are included when using your personal \
                  developer token. The first sync may take a moment for large annotation libraries.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(HYPOTHESIS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("hypothesis"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — composite "USERNAME::TOKEN").

/// Verify by calling `GET /api/profile` with the token (a 200 with the user's
/// profile confirms the token is valid). Store the token (0600) and the
/// username in the sync cursor.
fn def_connect(vault: &Vault, composite: &str) -> Result<()> {
    let (username, token) = split_composite(composite.trim())?;
    if username.is_empty() || token.is_empty() {
        bail!("paste as \"username::token\" — get your token from hypothes.is/account/developer");
    }
    // Verify: GET /api/profile must succeed with the token.
    let client = HypothesisClient::new(API_BASE.to_string());
    match client.get_profile(&token) {
        Ok(profile) => {
            // Double-check: the profile's userid should match the pasted username.
            let uid = profile
                .get("userid")
                .and_then(Value::as_str)
                .unwrap_or("");
            // userid format: "acct:USERNAME@hypothes.is" — tolerate any domain.
            if !uid.is_empty() {
                let in_userid = uid.split(':').nth(1).unwrap_or("").split('@').next().unwrap_or("");
                if !in_userid.is_empty() && in_userid != username {
                    bail!(
                        "token belongs to user '{}' but you entered username '{}' — \
                         check hypothes.is/account/developer",
                        in_userid, username
                    );
                }
            }
        }
        Err(FetchError::Unauthorized) => bail!(
            "Hypothesis rejected the token (401) — copy it fresh from hypothes.is/account/developer"
        ),
        Err(e) => bail!("Hypothesis auth check failed: {e}"),
    }
    // Store the token (secret, 0600).
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at: None,
        },
    )?;
    // Store the username in the non-secret cursor (it's not a secret).
    let mut state = vault.read_hypothesis_sync();
    state.username = username.to_string();
    vault.write_hypothesis_sync(&state)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        let state = vault.read_hypothesis_sync();
        let label = if state.username.is_empty() {
            "Hypothesis".to_string()
        } else {
            format!("@{}", state.username)
        };
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
///
/// The composite paste format is `USERNAME::TOKEN` — the username feeds the
/// `user=acct:USERNAME@hypothes.is` search param; the token is the Bearer
/// credential. Splitting on `::` is unambiguous because Hypothesis usernames
/// are `[a-zA-Z0-9_-]+` (no colons) and tokens are opaque hex strings.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "hypothesis",
    display_name: "Hypothesis",
    methods: &[ConnectMethod::TokenPaste {
        label: "Hypothesis username & token",
        help: "Paste as \"username::token\" — get your developer token from \
               hypothes.is/account/developer (it covers all annotations including \
               private groups, and is stored locally, never sent anywhere but Hypothesis).",
        placeholder: "myusername::abcdef1234567890…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["hypothesis"],
    setup: &[
        "Sign in to Hypothesis at hypothes.is.",
        "Open hypothes.is/account/developer to generate your personal API token.",
        "Copy your username (shown on the same page) and token.",
        "Paste as \"username::token\" here — it's stored locally.",
    ],
};

// ---------------------------------------------------------------------------
// Composite credential helper.

/// Split `"username::token"` → `(&str, &str)`. Bails with a clear message if
/// the separator is absent or either part is empty.
fn split_composite(s: &str) -> Result<(&str, &str)> {
    match s.split_once("::") {
        Some((u, t)) if !u.trim().is_empty() && !t.trim().is_empty() => Ok((u.trim(), t.trim())),
        Some((u, _)) if u.trim().is_empty() => bail!(
            "username is empty — paste as \"username::token\""
        ),
        Some(_) => bail!(
            "token is empty — paste as \"username::token\""
        ),
        None => bail!(
            "expected format is \"username::token\" (separated by \"::\")"
        ),
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Errors worth distinguishing at the call site.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// One page of search results.
struct Page {
    rows: Vec<Value>,
}

/// Endpoints the pull needs. Trait for test injection.
trait HypothesisApi {
    /// `GET /api/search` — one page of annotations, sorted by `id` ascending.
    /// `id_after` is the last `id` seen on the previous page; `None` starts
    /// from the beginning (full back-fill). Using `sort=id` guarantees forward
    /// progress even when many annotations share the same `updated` timestamp
    /// (bulk imports / group migrations) — a timestamp cursor with strict-`>`
    /// semantics would silently skip tied rows straddling a page boundary.
    fn search_page(
        &self,
        token: &str,
        user: &str,
        id_after: Option<&str>,
    ) -> Result<Page, FetchError>;

    /// `GET /api/profile` — verify the token.
    fn get_profile(&self, token: &str) -> Result<Value, FetchError>;
}

struct HypothesisClient {
    base: String,
}

impl HypothesisClient {
    fn new(base: String) -> Self {
        HypothesisClient { base }
    }

    fn bearer_get(&self, path: &str, token: &str) -> Result<Value, FetchError> {
        let url = format!("{}{path}", self.base);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .call()
        {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("JSON parse: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
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
}

impl HypothesisApi for HypothesisClient {
    fn search_page(
        &self,
        token: &str,
        user: &str,
        id_after: Option<&str>,
    ) -> Result<Page, FetchError> {
        // Paginate by `id` (unique per annotation) so that timestamp ties
        // (multiple annotations sharing the same `updated`) never cause rows
        // to be skipped — a `sort=updated` cursor with strict-`>` semantics
        // would silently lose rows that straddle a 200-row page boundary.
        let url = format!("{}/api/search", self.base);
        let user_param = format!("acct:{}@hypothes.is", user);
        let limit_str = PAGE_LIMIT.to_string();
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .query("user", &user_param)
            .query("sort", "id")
            .query("order", "asc")
            .query("limit", &limit_str);
        if let Some(id) = id_after {
            req = req.query("search_after", id);
        }
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("JSON parse: {e}")))?;
                let rows = v
                    .get("rows")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Ok(Page { rows })
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
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

    fn get_profile(&self, token: &str) -> Result<Value, FetchError> {
        self.bearer_get("/api/profile", token)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Hypothesis username (not a secret — used as the `user=acct:X@…` param).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    /// Max `updated` timestamp seen across the last full drain (RFC3339).
    /// Stored for telemetry / last-data only; pagination now uses `last_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<String>,
    /// Last annotation `id` returned on the final page of the previous drain.
    /// `sort=id&order=asc&search_after=<last_id>` picks up where we left off,
    /// guaranteeing forward progress regardless of `updated` timestamp ties.
    /// Old cursors without this field start from the beginning (full back-fill).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

impl Vault {
    fn read_hypothesis_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_hypothesis_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row — full-fidelity API object. Only `value` is serialized; `ts` is the
// partition key used by the stream helper but never written to disk.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// RFC3339-ish → RFC3339 local; passes through on parse failure (same as
/// readwise.rs — do not silently drop rows with slightly unusual timestamps).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Extract the quoted passage from `target[0].selector[]` — look for a
/// `TextQuoteSelector` (type == "TextQuoteSelector") and return its `exact`
/// field. Returns `""` when absent (e.g. a page-level annotation with no
/// selection, or a pure comment/reply).
///
/// Only `TextQuoteSelector` is recognised: other selector types
/// (`RangeSelector`, `TextPositionSelector`, etc.) may carry an `exact` field
/// but it does not represent the highlighted passage — surfacing it would
/// mislabel non-quote text as the contract `text` field.
fn extract_quote(annotation: &Value) -> String {
    let target = match annotation.get("target").and_then(Value::as_array) {
        Some(t) if !t.is_empty() => &t[0],
        _ => return String::new(),
    };
    let selectors = match target.get("selector").and_then(Value::as_array) {
        Some(s) => s,
        None => return String::new(),
    };
    for sel in selectors {
        if sel.get("type").and_then(Value::as_str) == Some("TextQuoteSelector") {
            let exact = sel.get("exact").and_then(Value::as_str).unwrap_or("").trim();
            if !exact.is_empty() {
                return exact.to_string();
            }
        }
    }
    String::new()
}

/// Extract the page title from `document.title` — the API returns it as
/// `string | string[]`; we take the first non-empty element.
fn extract_title(annotation: &Value) -> String {
    let doc = match annotation.get("document") {
        Some(d) => d,
        None => return String::new(),
    };
    match doc.get("title") {
        Some(Value::Array(arr)) => arr
            .iter()
            .find_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
            .unwrap_or("")
            .to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

/// Insert `k`→`v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// Map one Hypothesis API annotation object → a contract [`Highlight`].
/// Returns `None` when the row has no `id` (can't dedup) or no usable
/// timestamp (can't partition).
///
/// `ts` is set from `created` (the time the annotation was originally made),
/// NOT from `updated` (which reflects the most-recent edit). Using `created`
/// keeps the annotation at the right point on the reading timeline and matches
/// the readwise.rs precedent (`ts = highlighted_at`, `updated` used only as
/// the incremental cursor). Falls back to `updated` only when `created` is
/// absent or unparseable.
fn annotation_to_highlight(ann: &Value) -> Option<Highlight> {
    let guid = str_field(ann, "id");
    if guid.is_empty() {
        return None;
    }
    // Prefer `created` for `ts` — the annotation make-time. Fall back to
    // `updated` only when `created` is absent (should not happen in practice
    // but the API spec does not guarantee it).
    let raw_ts = {
        let c = str_field(ann, "created");
        if !c.is_empty() {
            c
        } else {
            let u = str_field(ann, "updated");
            if u.is_empty() {
                return None; // no usable timestamp → skip
            }
            u
        }
    };
    // Must yield a month partition; otherwise the row can't be filed.
    crate::store::Partition::Month.key(&to_local(&raw_ts))?;
    let ts = to_local(&raw_ts);

    let text = extract_quote(ann);  // the highlighted passage
    let note = str_field(ann, "text"); // the user's annotation body

    let mut extra = Map::new();
    put_str(&mut extra, "group", &str_field(ann, "group"));
    // incontext link — the annotation in its source page context.
    if let Some(incontext) = ann
        .get("links")
        .and_then(|l| l.get("incontext"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        extra.insert("incontext".into(), Value::String(incontext.to_string()));
    }
    // `updated` → extra: keeps the edit-time for incremental-pull reference
    // without polluting `ts` with it.
    let updated = str_field(ann, "updated");
    if !updated.is_empty() {
        put_str(&mut extra, "updated", &to_local(&updated));
    }

    Some(Highlight {
        ts,
        source: "hypothesis".into(),
        guid,
        text,
        note,
        title: extract_title(ann),
        author: String::new(), // Hypothesis is web annotations — no author field
        url: str_field(ann, "uri"),
        location: String::new(), // no location concept in Hypothesis web annotations
        color: String::new(),    // Hypothesis does not expose highlight color via API
        tags: ann
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(str::trim).filter(|s| !s.is_empty()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid.

/// Append new contract + raw rows, deduped by guid. Returns the number of new
/// contract rows written.
fn write_layer(vault: &Vault, rows: Vec<(Highlight, Value)>) -> Result<u64> {
    let contract = vault.stream(HIGHLIGHTS_DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Scan existing guids from the contract stream (re-runnable, no dups).
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<Highlight> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (row, raw_val) in rows {
        if row.guid.is_empty() || !seen.insert(row.guid.clone()) {
            continue;
        }
        new_raws.push(RawLine { ts: row.ts.clone(), value: raw_val });
        new_rows.push(row);
    }

    contract.append(&new_rows, |r| &r.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the token + username, sync. Missing token → a quiet skip on the
/// periodic path; a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Hypothesis is not connected — add your username and token in the Integrations tab")?;
    let state = vault.read_hypothesis_sync();
    if state.username.is_empty() {
        bail!("Hypothesis username is missing — reconnect from the Integrations tab");
    }
    let client = HypothesisClient::new(API_BASE.to_string());
    pull_with(vault, &client, &token, &state.username)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(
    vault: &Vault,
    api: &impl HypothesisApi,
    token: &str,
    username: &str,
) -> Result<PullOutcome> {
    let mut state = vault.read_hypothesis_sync();
    state.username = username.to_string();

    // Drain all pages starting after the last-seen id (sort=id cursor).
    // We collect everything before writing — NEVER advance the watermark on a
    // partial drain (readwise.rs / todoist.rs precedent).
    let (annotations, new_last_id) =
        drain_all(api, token, username, state.last_id.as_deref())
            .map_err(|e| match e {
                FetchError::Unauthorized => anyhow::anyhow!(
                    "Hypothesis rejected the token — reconnect from the Integrations tab"
                ),
                FetchError::RateLimited => anyhow::anyhow!(
                    "Hypothesis rate limited the search endpoint — it'll retry on the next sync"
                ),
                other => anyhow::anyhow!("Hypothesis search fetch failed: {other}"),
            })?;

    // Compute the new `updated` watermark before writing (in case writing errors).
    // Used for telemetry / last-data; pagination now uses `last_id` above.
    let new_watermark = annotations
        .iter()
        .filter_map(|a| {
            let u = str_field(a, "updated");
            (!u.is_empty()).then_some(u)
        })
        .max();

    // Map + write.
    let rows: Vec<(Highlight, Value)> = annotations
        .iter()
        .filter_map(|a| annotation_to_highlight(a).map(|h| (h, a.clone())))
        .collect();
    let n = write_layer(vault, rows)?;

    // Advance cursors only after a successful write, and only forward.
    if let Some(id) = new_last_id {
        // `last_id` always advances to the newest id seen (sort=id is strictly
        // monotone so this is always forward).
        state.last_id = Some(id);
    }
    if let Some(w) = new_watermark {
        if state.updated_after.as_deref().is_none_or(|cur| w.as_str() > cur) {
            state.updated_after = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_hypothesis_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("highlights", n);
    Ok(PullOutcome {
        headline: format!("{n} annotations"),
        counts,
    })
}

/// Drain all pages of the search endpoint using `sort=id&order=asc`.
///
/// `id_after` is the `id` of the last annotation returned on the previous
/// run's final page (`SyncState::last_id`). Using annotation `id` as the
/// pagination cursor guarantees strict forward progress: each page returns
/// rows with `id > id_after`, so the cursor always advances by at least one
/// position. This avoids the timestamp-tie data-loss bug that `sort=updated`
/// cursors have: when >200 annotations share the same `updated` timestamp
/// (bulk import / group migration), a strict-`>` updated cursor skips all
/// rows beyond the first 200.
///
/// Returns `(rows, last_id)` — `last_id` is the `id` of the final row
/// returned (to be persisted as the next `id_after`), or `None` when the
/// drain returns no rows.
///
/// The whole drain either succeeds or fails atomically — the caller does NOT
/// advance the watermark on a partial failure (the `?` propagates the error
/// before any write).
fn drain_all(
    api: &impl HypothesisApi,
    token: &str,
    username: &str,
    id_after: Option<&str>,
) -> Result<(Vec<Value>, Option<String>), FetchError> {
    let mut all: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = id_after.map(str::to_string);
    let mut last_id: Option<String> = cursor.clone();

    loop {
        let page = api.search_page(token, username, cursor.as_deref())?;
        let n = page.rows.len();
        // Advance the cursor to the `id` of the last row on this page so the
        // next request starts strictly after it.
        if let Some(last_row) = page.rows.last() {
            let id = str_field(last_row, "id");
            if !id.is_empty() {
                last_id = Some(id.clone());
                cursor = Some(id);
            }
        }
        all.extend(page.rows);
        if n < PAGE_LIMIT {
            break; // last page (empty or short — no more rows)
        }
        // cursor was advanced above; loop continues to the next page.
    }
    Ok((all, last_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-hypothesis-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — shaped from the confirmed API response structure.

    /// A typical annotation with a TextQuoteSelector, note, tags, and group.
    fn ann_full() -> Value {
        json!({
            "id": "AaBbCc-annot-01",
            "created": "2026-06-07T11:25:00.000000+00:00",
            "updated": "2026-06-07T11:30:00.000000+00:00",
            "uri": "https://example.com/korzybski-essay",
            "text": "Useful framing for the data-model section.",
            "tags": ["epistemics", "data"],
            "group": "__world__",
            "user": "acct:testuser@hypothes.is",
            "target": [{
                "source": "https://example.com/korzybski-essay",
                "selector": [
                    {
                        "type": "RangeSelector",
                        "startContainer": "/div[1]/p[3]",
                        "endContainer": "/div[1]/p[3]",
                        "startOffset": 0,
                        "endOffset": 30
                    },
                    {
                        "type": "TextQuoteSelector",
                        "exact": "The map is not the territory.",
                        "prefix": "As Korzybski wrote, ",
                        "suffix": " This has implications"
                    }
                ]
            }],
            "document": {
                "title": ["On General Semantics"]
            },
            "links": {
                "incontext": "https://hyp.is/AaBbCc-annot-01/example.com/korzybski-essay",
                "json": "https://api.hypothes.is/api/annotations/AaBbCc-annot-01"
            }
        })
    }

    /// A page-level annotation (no selector / no TextQuoteSelector) — just a note.
    fn ann_no_quote() -> Value {
        json!({
            "id": "DdEeFf-annot-02",
            "created": "2026-05-30T09:00:00.000000+00:00",
            "updated": "2026-05-30T09:05:00.000000+00:00",
            "uri": "https://example.com/another-page",
            "text": "This whole page is interesting.",
            "tags": [],
            "group": "private-group-xyz",
            "user": "acct:testuser@hypothes.is",
            "target": [{"source": "https://example.com/another-page"}],
            "document": {"title": ["Another Page Title"]},
            "links": {}
        })
    }

    /// An annotation with a multi-element title array.
    fn ann_array_title() -> Value {
        json!({
            "id": "GgHhIi-annot-03",
            "created": "2026-06-10T14:00:00.000000+00:00",
            "updated": "2026-06-10T14:03:00.000000+00:00",
            "uri": "https://example.com/deep-dive",
            "text": "",
            "tags": ["reading"],
            "group": "__world__",
            "user": "acct:testuser@hypothes.is",
            "target": [{
                "source": "https://example.com/deep-dive",
                "selector": [{
                    "type": "TextQuoteSelector",
                    "exact": "Local-first software is the future.",
                    "prefix": "In conclusion, ",
                    "suffix": ""
                }]
            }],
            "document": {
                "title": ["", "A Deep Dive into Local-First Software"]
            },
            "links": {}
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_full_annotation_to_highlight() {
        let h = annotation_to_highlight(&ann_full()).unwrap();
        assert_eq!(h.source, "hypothesis");
        assert_eq!(h.guid, "AaBbCc-annot-01");
        assert_eq!(h.text, "The map is not the territory.", "quote from TextQuoteSelector.exact");
        assert_eq!(h.note, "Useful framing for the data-model section.", "note from text field");
        assert_eq!(h.title, "On General Semantics");
        assert_eq!(h.url, "https://example.com/korzybski-essay");
        assert_eq!(h.tags, vec!["epistemics", "data"]);
        assert_eq!(h.extra.get("group"), Some(&json!("__world__")));
        assert!(
            h.extra.contains_key("incontext"),
            "incontext link must be in extra"
        );
        assert_eq!(
            h.extra.get("incontext").and_then(Value::as_str),
            Some("https://hyp.is/AaBbCc-annot-01/example.com/korzybski-essay")
        );
        // ts is `created` (annotation make-time), NOT `updated` (edit-time).
        assert_eq!(
            DateTime::parse_from_rfc3339(&h.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-07T11:25:00.000000+00:00")
                .unwrap()
                .timestamp(),
            "ts must reflect created (11:25), not updated (11:30)"
        );
        // `updated` is preserved in extra so incremental cursor still works.
        assert!(
            h.extra.contains_key("updated"),
            "updated must be in extra (incremental cursor reference)"
        );
    }

    #[test]
    fn maps_page_level_annotation_no_quote() {
        let h = annotation_to_highlight(&ann_no_quote()).unwrap();
        assert_eq!(h.guid, "DdEeFf-annot-02");
        assert!(h.text.is_empty(), "no TextQuoteSelector → empty text field");
        assert_eq!(h.note, "This whole page is interesting.");
        assert_eq!(h.title, "Another Page Title");
        assert!(h.tags.is_empty());
        assert_eq!(h.extra.get("group"), Some(&json!("private-group-xyz")));
        // omit-empty: text is empty so it must not appear in serialization.
        let re = serde_json::to_value(&h).unwrap();
        assert!(re.get("text").is_none(), "empty text omitted in output");
    }

    #[test]
    fn maps_annotation_with_array_title() {
        let h = annotation_to_highlight(&ann_array_title()).unwrap();
        assert_eq!(
            h.title, "A Deep Dive into Local-First Software",
            "first non-empty element of title array"
        );
        assert_eq!(h.text, "Local-first software is the future.");
    }

    #[test]
    fn annotation_with_no_id_yields_none() {
        let mut a = ann_full();
        a.as_object_mut().unwrap().remove("id");
        assert!(annotation_to_highlight(&a).is_none(), "no id → skip");
    }

    #[test]
    fn annotation_with_no_created_uses_updated_fallback() {
        // When `created` is absent, `updated` is the fallback timestamp.
        let mut a = ann_full();
        a.as_object_mut().unwrap().remove("created");
        let h = annotation_to_highlight(&a).expect("fallback to updated should succeed");
        // ts should equal `updated` (11:30) when `created` is absent.
        assert_eq!(
            DateTime::parse_from_rfc3339(&h.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-07T11:30:00.000000+00:00")
                .unwrap()
                .timestamp(),
            "fallback ts = updated when created is absent"
        );
    }

    #[test]
    fn annotation_with_no_timestamps_yields_none() {
        // Both `created` and `updated` absent → no usable timestamp → skip.
        let mut a = ann_full();
        a.as_object_mut().unwrap().remove("created");
        a.as_object_mut().unwrap().remove("updated");
        assert!(annotation_to_highlight(&a).is_none(), "no timestamps → skip");
    }

    #[test]
    fn extract_quote_skips_non_textquote_selectors() {
        let ann = json!({
            "id": "x", "created": "2026-01-01T00:00:00+00:00", "updated": "2026-01-01T00:00:00+00:00",
            "uri": "", "text": "", "tags": [], "group": "",
            "target": [{
                "source": "https://example.com",
                "selector": [
                    {"type": "RangeSelector", "exact": "range-text"},
                    {"type": "TextPositionSelector", "start": 0, "end": 5}
                ]
            }],
            "document": {}
        });
        // RangeSelector and TextPositionSelector carry an `exact` field but
        // they do NOT represent the highlighted passage. The fallback that
        // would pick `exact` from any selector was removed (minor defect fix):
        // only TextQuoteSelector.exact is the contract `text` field.
        let q = extract_quote(&ann);
        assert!(q.is_empty(), "non-TextQuote selectors must not populate the quote field");
    }

    #[test]
    fn split_composite_parses_and_rejects() {
        assert!(split_composite("user::tok").is_ok());
        let (u, t) = split_composite("myname::abc123def456").unwrap();
        assert_eq!(u, "myname");
        assert_eq!(t, "abc123def456");
        // Whitespace is trimmed.
        let (u2, t2) = split_composite("  user2  ::  tok2  ").unwrap();
        assert_eq!(u2, "user2");
        assert_eq!(t2, "tok2");
        // Rejections.
        assert!(split_composite("nodelimiter").is_err());
        assert!(split_composite("::tok").is_err(), "empty username");
        assert!(split_composite("user::").is_err(), "empty token");
    }

    // -----------------------------------------------------------------------
    // Mock API + integration tests.

    struct MockApi {
        pages: RefCell<VecDeque<Result<Page, FetchError>>>,
        /// Captures the `id_after` cursors passed to each `search_page` call,
        /// so pagination tests can assert the correct cursor sequence.
        cursors_seen: RefCell<Vec<Option<String>>>,
        profile: Option<Value>,
    }

    impl MockApi {
        fn with_pages(pages: Vec<Vec<Value>>) -> Self {
            let mut q: VecDeque<Result<Page, FetchError>> = VecDeque::new();
            for p in pages {
                q.push_back(Ok(Page { rows: p }));
            }
            MockApi {
                pages: RefCell::new(q),
                cursors_seen: RefCell::new(Vec::new()),
                profile: Some(json!({"userid": "acct:testuser@hypothes.is"})),
            }
        }
    }

    impl HypothesisApi for MockApi {
        fn search_page(
            &self,
            _token: &str,
            _user: &str,
            id_after: Option<&str>,
        ) -> Result<Page, FetchError> {
            self.cursors_seen.borrow_mut().push(id_after.map(str::to_string));
            self.pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(Page { rows: vec![] }))
        }
        fn get_profile(&self, _token: &str) -> Result<Value, FetchError> {
            match &self.profile {
                Some(p) => Ok(p.clone()),
                None => Err(FetchError::Unauthorized),
            }
        }
    }

    #[test]
    fn full_pull_writes_contract_and_raw_dedupes_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::with_pages(vec![vec![ann_full(), ann_no_quote(), ann_array_title()]]);

        let out = pull_with(&v, &api, "tok", "testuser").unwrap();
        assert_eq!(out.counts.get("highlights"), Some(&3));

        // Highlights partitioned by `created` month (major defect fix: ts = created).
        // ann_full:       created 2026-06-07 → June
        // ann_no_quote:   created 2026-05-30 → May
        // ann_array_title: created 2026-06-10 → June
        let jun =
            std::fs::read_to_string(v.root().join("reading/hypothesis/highlights/2026-06.jsonl"))
                .unwrap();
        let may =
            std::fs::read_to_string(v.root().join("reading/hypothesis/highlights/2026-05.jsonl"))
                .unwrap();
        assert_eq!(jun.lines().count(), 2, "two June annotations");
        assert_eq!(may.lines().count(), 1, "one May annotation");
        assert!(jun.contains("\"guid\":\"AaBbCc-annot-01\""));
        assert!(jun.contains("\"guid\":\"GgHhIi-annot-03\""));

        // `ts` must reflect `created`, not `updated`.
        let jun_parsed: Vec<Value> = jun
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let ann_full_row = jun_parsed.iter().find(|r| r["guid"] == "AaBbCc-annot-01").unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(ann_full_row["ts"].as_str().unwrap())
                .unwrap()
                .timestamp(),
            DateTime::parse_from_rfc3339("2026-06-07T11:25:00.000000+00:00")
                .unwrap()
                .timestamp(),
            "ts = created (11:25), not updated (11:30)"
        );
        // `updated` must be in extra.
        assert!(
            ann_full_row.get("extra").and_then(|e| e.get("updated")).is_some(),
            "updated must be preserved in extra"
        );

        // Raw layer mirrors partitioning by `created` (raw row's ts = contract ts = created).
        // ann_full (created 2026-06-07) and ann_array_title (created 2026-06-10) → June raw.
        let raw_jun =
            std::fs::read_to_string(v.root().join("reading/hypothesis/raw/2026-06.jsonl")).unwrap();
        assert!(raw_jun.contains("\"id\":\"AaBbCc-annot-01\""), "raw keeps full API object");
        assert!(raw_jun.contains("RangeSelector"), "raw keeps non-contract fields");

        // Updated watermark advanced to max `updated` across the drain.
        let state = v.read_hypothesis_sync();
        assert_eq!(
            state.updated_after.as_deref(),
            Some("2026-06-10T14:03:00.000000+00:00"),
            "updated_after = max updated across drain"
        );
        // last_id cursor set to the id of the final row in the page.
        assert!(state.last_id.is_some(), "last_id must be set after drain");
        assert_eq!(state.last_id.as_deref(), Some("GgHhIi-annot-03"),
            "last_id = id of the last row on the final page");
        assert!(state.updated.is_some(), "sync timestamp written");
        // Username stored in cursor.
        assert_eq!(state.username, "testuser");
        // Token never in cursor file.
        let cursor_file = std::fs::read_to_string(v.root().join(".trove/hypothesis-sync.json")).unwrap();
        assert!(!cursor_file.contains("tok"), "token never in cursor");

        // Re-run with the same annotations → guid dedupe, counts all-zero.
        let api2 = MockApi::with_pages(vec![vec![ann_full(), ann_no_quote(), ann_array_title()]]);
        let out2 = pull_with(&v, &api2, "tok", "testuser").unwrap();
        assert_eq!(out2.counts.get("highlights"), Some(&0), "dedup on re-run");
        let jun2 = std::fs::read_to_string(
            v.root().join("reading/hypothesis/highlights/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(jun, jun2, "file byte-identical after re-run");
    }

    #[test]
    fn empty_first_page_writes_nothing_and_no_watermark_advance() {
        let v = temp_vault("empty");
        let api = MockApi::with_pages(vec![vec![]]);
        let out = pull_with(&v, &api, "tok", "testuser").unwrap();
        assert_eq!(out.counts.get("highlights"), Some(&0));
        let state = v.read_hypothesis_sync();
        assert!(state.updated_after.is_none(), "no drain → updated_after untouched");
        assert!(state.last_id.is_none(), "no drain → last_id untouched");
    }

    #[test]
    fn pull_requires_username() {
        let v = temp_vault("nousername");
        // Store the token but leave the sync file username-less.
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "tok".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("username"), "clear error when username missing: {err}");
    }

    #[test]
    fn pull_requires_token() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error when token missing: {err}");
    }

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        // First-sync: empty file deserializes to all-defaults.
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.username.is_empty());
        assert!(empty.updated_after.is_none());
        assert!(empty.last_id.is_none());
        // Older cursor without `last_id` (pre-fix) still deserializes cleanly.
        let old: SyncState =
            serde_json::from_str(r#"{"updated_after":"2026-01-01T00:00:00+00:00"}"#).unwrap();
        assert_eq!(old.updated_after.as_deref(), Some("2026-01-01T00:00:00+00:00"));
        assert!(old.username.is_empty());
        assert!(old.last_id.is_none(), "old cursor without last_id must default to None");
        // Cursor with all fields roundtrips.
        let full = SyncState {
            username: "alice".into(),
            updated_after: Some("2026-06-01T00:00:00+00:00".into()),
            last_id: Some("abc123".into()),
            updated: Some("2026-06-10T00:00:00+00:00".into()),
        };
        let s = serde_json::to_string(&full).unwrap();
        let back: SyncState = serde_json::from_str(&s).unwrap();
        assert_eq!(back.last_id.as_deref(), Some("abc123"));
    }

    /// Regression: multi-page pagination with timestamp ties must not drop rows.
    ///
    /// This test exercises the blocking defect that a `sort=updated` cursor with
    /// strict-`>` semantics would cause: when a full page of PAGE_LIMIT rows all
    /// share one `updated` timestamp (bulk import / group migration), the old
    /// cursor would advance to that timestamp and the NEXT request would return
    /// `updated > T`, skipping all rows 201-400 that also carry `updated = T`.
    ///
    /// With `sort=id` the cursor advances by `id`, guaranteeing zero loss.
    #[test]
    fn multipage_pagination_with_timestamp_ties_writes_all_rows() {
        // Build PAGE_LIMIT rows for page 1, all sharing the same `updated`.
        // 10 more rows for page 2, also sharing the same `updated`.
        // With a `sort=updated` cursor, page 2 would be skipped.
        // With a `sort=id` cursor, page 2 is fetched correctly.
        let tie_updated = "2026-06-15T10:00:00.000000+00:00";
        let tie_created = "2026-06-15T09:00:00.000000+00:00";

        let page1: Vec<Value> = (0..PAGE_LIMIT)
            .map(|i| {
                json!({
                    "id": format!("bulk-p1-{:04}", i),
                    "created": tie_created,
                    "updated": tie_updated,
                    "uri": format!("https://example.com/page1/{}", i),
                    "text": format!("note {}", i),
                    "tags": [],
                    "group": "__world__",
                    "user": "acct:testuser@hypothes.is",
                    "target": [{"source": format!("https://example.com/page1/{}", i)}],
                    "document": {"title": [format!("Page 1 Doc {}", i)]}
                })
            })
            .collect();

        let page2: Vec<Value> = (0..10usize)
            .map(|i| {
                json!({
                    "id": format!("bulk-p2-{:04}", i),
                    "created": tie_created,
                    "updated": tie_updated,
                    "uri": format!("https://example.com/page2/{}", i),
                    "text": format!("note p2 {}", i),
                    "tags": [],
                    "group": "__world__",
                    "user": "acct:testuser@hypothes.is",
                    "target": [{"source": format!("https://example.com/page2/{}", i)}],
                    "document": {"title": [format!("Page 2 Doc {}", i)]}
                })
            })
            .collect();

        let v = temp_vault("multipage_tie");
        let api = MockApi::with_pages(vec![page1, page2]);

        let out = pull_with(&v, &api, "tok", "testuser").unwrap();
        assert_eq!(
            out.counts.get("highlights"),
            Some(&(PAGE_LIMIT as u64 + 10)),
            "all {} rows must be written — none dropped by timestamp-tie pagination",
            PAGE_LIMIT + 10
        );

        // Verify the cursor advanced correctly: page 1 cursor = last id of page 1.
        let state = v.read_hypothesis_sync();
        assert_eq!(
            state.last_id.as_deref(),
            Some("bulk-p2-0009"),
            "last_id must be the final row id from the last page"
        );
        assert_eq!(
            *api.cursors_seen.borrow(),
            vec![
                None,                                // first call: no prior cursor
                Some("bulk-p1-0199".to_string()),    // second call: last id of page 1
            ],
            "second page must be requested with the last id of page 1 as cursor"
        );
    }

    #[test]
    fn def_connection_exposes_token_paste_and_correct_ids() {
        assert_eq!(CONNECTION.id, "hypothesis");
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(DEF.connection, Some("hypothesis"));
    }

    #[test]
    fn def_behavior_is_periodic() {
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
    }
}
