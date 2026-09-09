//! Harvest — cloud time-tracking and invoicing service with a v2 REST API.
//! A **Periodic** pull of the user's time entries into the bound
//! [`crate::time_entries`] contract (same domain as Toggl Track and Clockify;
//! writes `time-entries/harvest/YYYY-MM.jsonl`). Catalogued in the Phase 2
//! pass; brief: docs/integrations/harvest.md.
//!
//! ## Auth — two required headers
//!
//! Harvest v2 requires BOTH `Authorization: Bearer <PAT>` AND
//! `Harvest-Account-Id: <id>` on every request. The user pastes a single
//! composite credential in the form `<PAT>|<account-id>` (separator `|`), which
//! the connect handler splits. Both values are stored together as a single
//! 0600 token set under `.trove/sync/harvest`.
//!
//! ## Time entries endpoint & pagination
//!
//! `GET https://api.harvestapp.com/v2/time_entries`
//!
//! - `updated_since` (ISO 8601 UTC datetime) bounds the window incrementally.
//! - Results come in pages; the response `links.next` URL drives pagination
//!   (follow until `links.next` is null).
//! - Default (and max) `per_page` is 2000, so the first call often drains all.
//!
//! ## Vault mapping
//!
//! - **Raw** `time-entries/harvest/raw/YYYY-MM.jsonl` — verbatim v2 objects,
//!   deduped by (`id`, `updated_at`) so running->stopped transitions are
//!   preserved while identical re-polls are idempotent.
//! - **Contract** `time-entries/harvest/YYYY-MM.jsonl` — one
//!   [`crate::time_entries::TimeEntry`] per entry; append-only, first-observed-id
//!   wins (same Toggl/Clockify convention).
//!
//! ## Field mapping
//!
//! Unlike Toggl, the Harvest response embeds the full project/client/task objects
//! so no secondary lookup is needed. `start` uses the date-only `spent_date`
//! (Harvest guarantees only YYYY-MM-DD per entry; `started_time`/`ended_time` are
//! 12-hour time strings without timezone, so they are kept in `extra` only).
//! `duration_secs` is `round(hours * 3600)`. Source-specific bits (rates,
//! invoice id, approval status, budget flags, ...) ride in `extra`.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::time_entries::TimeEntry;
use crate::vault::Vault;

const DIR: &str = "time-entries/harvest";
const RAW_DIR: &str = "time-entries/harvest/raw";
const SYNC_FILE: &str = ".trove/harvest-sync.json";
const SERVICE: &str = "harvest";
const API_BASE: &str = "https://api.harvestapp.com/v2";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hourly -- matches Toggl/Clockify cadence; the Harvest API is generous.
const SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!("harvest synced -- {} time entries", c("entries"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "harvest sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Harvest synced -- {} time entries", c("entries")),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "harvest",
        name: "Harvest",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your personal time entries from Harvest via the v2 API \
                      (api.harvestapp.com/v2/time_entries). Syncs hourly; uses an incremental \
                      updated_since cursor so only changed entries are re-fetched after the \
                      first full pull.",
        domain: "time-entries",
        vault_path: "time-entries/harvest/",
        toggleable: true,
        setup: &[
            "Connect with your Harvest personal access token and Account ID on this card.",
            "First sync pulls your full time-entry history; later syncs are incremental.",
        ],
        caveats: "Requires both a personal access token and a Harvest Account ID -- both from \
                  id.getharvest.com/developers. Member accounts see only their own tracked time. \
                  Note: this is the time-tracking Harvest (harvestapp.com), unrelated to \
                  Greenhouse's recruiting product of the same name.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("harvest"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection -- TokenPaste with a composite credential: `<PAT>|<account-id>`.

/// Parse a composite credential: `<PAT>|<account-id>` or `<PAT>\n<account-id>`.
/// Returns `(token, account_id)`. `None` when either part is empty or missing.
fn parse_composite(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    // Accept pipe or newline as the separator (copy-paste may vary).
    let sep_pos = raw.find('|').or_else(|| raw.find('\n'))?;
    let pat = raw[..sep_pos].trim().to_string();
    let acc = raw[sep_pos + 1..].trim().to_string();
    if pat.is_empty() || acc.is_empty() {
        return None;
    }
    Some((pat, acc))
}

fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let (token, account_id) = parse_composite(raw).ok_or_else(|| {
        anyhow::anyhow!(
            "paste your Harvest Personal Access Token and Account ID separated by '|', e.g.: \
             MyToken123|1234567. Both are found at id.getharvest.com/developers."
        )
    })?;

    // Verify with a lightweight /users/me call.
    let client = HarvestClient::new(API_BASE.to_string(), token.clone(), account_id.clone());
    match client.verify() {
        Ok(()) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Harvest rejected the credentials (401) -- re-copy the token and Account ID from \
             id.getharvest.com/developers"
        ),
        Err(e) => bail!("Harvest auth check failed: {e}"),
    }

    // Store the composite as `access_token = "<token>|<account_id>"`.
    // The run-time always re-parses it; both halves are secrets.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: format!("{token}|{account_id}"),
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Harvest".to_string(),
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
    id: "harvest",
    display_name: "Harvest",
    methods: &[ConnectMethod::TokenPaste {
        label: "Personal Access Token | Account ID",
        help: "Paste your Harvest Personal Access Token and Account ID separated by '|'. \
               Both are generated at id.getharvest.com/developers under 'Personal Access Tokens'.",
        placeholder: "eyJhbGc...|1234567",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["harvest"],
    setup: &[
        "Sign in to your Harvest account, then go to id.getharvest.com/developers.",
        "Under 'Personal Access Tokens', create a new token and note your Account ID.",
        "Paste them here separated by '|', e.g.: MyToken123|1234567.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer -- injectable so tests run fully offline.

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

/// The page the time-entries endpoint returns.
#[derive(Deserialize)]
struct Page {
    time_entries: Vec<Value>,
    links: Links,
}

#[derive(Deserialize)]
struct Links {
    next: Option<String>,
}

trait HarvestApi {
    /// Fetch one page of time entries.
    ///
    /// - On the **first** call: `next_url` is `None`; `updated_since` bounds
    ///   the window. The client builds `GET /v2/time_entries?per_page=2000[&updated_since=…]`.
    /// - On **subsequent** calls: `next_url` is `Some(url)` — the absolute URL
    ///   from `links.next` in the previous response. **Follow it verbatim.**
    ///   Harvest may use cursor-based pagination in which `?page=N` would
    ///   repeat or strand results.
    ///
    /// Returns `(entries, next_url)`.
    fn time_entries_page(
        &self,
        updated_since: Option<&str>,
        next_url: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>), FetchError>;
}

struct HarvestClient {
    base: String,
    token: String,
    account_id: String,
}

impl HarvestClient {
    fn new(base: String, token: String, account_id: String) -> Self {
        HarvestClient { base, token, account_id }
    }

    fn set_headers(&self, req: ureq::Request) -> ureq::Request {
        req.set("Authorization", &format!("Bearer {}", self.token))
            .set("Harvest-Account-Id", &self.account_id)
            .set("User-Agent", "Trove (https://github.com/trove-app/trove)")
    }

    fn verify(&self) -> Result<(), FetchError> {
        let url = format!("{}/users/me", self.base);
        let req = self.set_headers(ureq::get(&url).timeout(HTTP_TIMEOUT));
        match req.call() {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl HarvestApi for HarvestClient {
    fn time_entries_page(
        &self,
        updated_since: Option<&str>,
        next_url: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>), FetchError> {
        // If the previous page returned a `links.next` URL, follow it verbatim
        // (Harvest may be cursor-based; constructing ?page=N would strand entries).
        // On the first call build the initial URL ourselves.
        let req = if let Some(url) = next_url {
            self.set_headers(ureq::get(url).timeout(HTTP_TIMEOUT))
        } else {
            let url = format!("{}/time_entries", self.base);
            let mut r = self
                .set_headers(ureq::get(&url).timeout(HTTP_TIMEOUT))
                .query("per_page", "2000");
            if let Some(since) = updated_since {
                r = r.query("updated_since", since);
            }
            r
        };
        match req.call() {
            Ok(resp) => {
                let page_body: Page = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok((page_body.time_entries, page_body.links.next))
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
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// ISO 8601 UTC datetime of the max `updated_at` seen in the last full
    /// drain -- the `updated_since` lower bound for the next poll. `None` on a
    /// first sync (no lower bound, pull everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_since: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_harvest_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_harvest_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row (verbatim API object, keyed by (id, updated_at) for idempotence).

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping.

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn nested_str(v: &Value, obj: &str, key: &str) -> String {
    v.get(obj)
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Hours (decimal) -> integer seconds, rounded to nearest.
fn hours_to_secs(hours: f64) -> i64 {
    (hours * 3600.0).round() as i64
}

/// The raw-layer dedupe key: (`id`, `updated_at`). Returns `None` without an id.
fn raw_key(v: &Value) -> Option<(String, String)> {
    let id = match v.get("id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return None,
    };
    Some((id, str_field(v, "updated_at")))
}

/// A v2 time-entry object -> a contract [`TimeEntry`]. Returns `None` when
/// required fields are absent (no id, or no `spent_date` that can be
/// month-partitioned).
fn entry_from(e: &Value) -> Option<TimeEntry> {
    // `id` -- stable bigint in Harvest v2 (documented), serialized as string.
    let id = match e.get("id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return None,
    };

    // `spent_date` is YYYY-MM-DD -- the partition key. Date-only `start` is
    // explicitly supported by the TimeEntry schema (see time_entries.rs).
    let start = str_field(e, "spent_date");
    if start.is_empty() {
        return None;
    }
    // Must yield a valid month partition.
    Partition::Month.key(&start)?;

    // Running timer: `is_running: true` -> no duration_secs emitted.
    // Stopped entries with hours==0 emit Some(0) so they are distinguishable
    // from a running timer (which has no duration_secs at all).
    let is_running = e.get("is_running").and_then(Value::as_bool).unwrap_or(false);

    let duration_secs = if is_running {
        None
    } else {
        // For stopped entries, always emit Some(_) even when hours==0.0.
        e.get("hours")
            .and_then(Value::as_f64)
            .map(hours_to_secs)
    };

    // Harvest embeds project/client/task as `{"id": ..., "name": ...}` objects
    // -- no secondary lookup needed. `notes` is the description field.
    let project = nested_str(e, "project", "name");
    let client = nested_str(e, "client", "name");
    let task = nested_str(e, "task", "name");
    let description = str_field(e, "notes");
    let billable = e.get("billable").and_then(Value::as_bool);

    // Source-specific extras (ids, rates, invoice, approval, ...).
    let mut extra = Map::new();

    macro_rules! put_i64 {
        ($obj:expr, $key:expr, $xk:expr) => {
            if let Some(v) = e.get($obj).and_then(|o| o.get($key)).and_then(Value::as_i64) {
                extra.insert($xk.into(), Value::from(v));
            }
        };
    }
    macro_rules! put_f64 {
        ($field:expr, $xk:expr) => {
            if let Some(v) = e.get($field).and_then(Value::as_f64) {
                extra.insert($xk.into(), Value::from(v));
            }
        };
    }
    macro_rules! put_bool {
        ($field:expr) => {
            if let Some(v) = e.get($field).and_then(Value::as_bool) {
                extra.insert($field.into(), Value::Bool(v));
            }
        };
    }
    macro_rules! put_str {
        ($field:expr) => {
            let s = str_field(e, $field);
            if !s.is_empty() {
                extra.insert($field.into(), Value::String(s));
            }
        };
    }

    put_i64!("project", "id", "project_id");
    put_i64!("client",  "id", "client_id");
    put_i64!("task",    "id", "task_id");
    put_i64!("user",    "id", "user_id");

    put_f64!("hours",               "hours");
    put_f64!("rounded_hours",       "rounded_hours");
    put_f64!("hours_without_timer", "hours_without_timer");
    put_f64!("billable_rate",       "billable_rate");
    put_f64!("cost_rate",           "cost_rate");

    // Invoice reference.
    if let Some(inv) = e.get("invoice") {
        if let Some(inv_id) = inv.get("id").and_then(Value::as_i64) {
            extra.insert("invoice_id".into(), Value::from(inv_id));
        }
        if let Some(inv_num) = inv.get("number").and_then(Value::as_str) {
            if !inv_num.trim().is_empty() {
                extra.insert("invoice_number".into(), Value::String(inv_num.trim().into()));
            }
        }
    }

    put_bool!("is_billed");
    put_bool!("is_locked");
    put_bool!("budgeted");
    put_str!("approval_status");
    put_str!("timer_started_at");
    put_str!("started_time");
    put_str!("ended_time");
    put_str!("updated_at");

    Some(TimeEntry {
        source: "harvest".into(),
        id,
        start,
        end: String::new(), // Harvest is date-only; no usable end timestamp
        duration_secs,
        description,
        project,
        client,
        task,
        tags: Vec::new(), // Harvest v2 has no tag field on time entries
        billable,
        extra,
    })
}

/// Max `updated_at` across a slice of entry references, normalized to UTC RFC3339.
/// Used as the `updated_since` cursor.
///
/// `pull_with` calls this over **successfully-mapped entries only** so that a
/// parse miss (entry_from returns None) never silently advances the cursor past
/// an unwritten entry.
fn max_updated_at_refs(entries: &[&Value]) -> Option<String> {
    entries
        .iter()
        .filter_map(|e| {
            let s = str_field(e, "updated_at");
            if s.is_empty() {
                None
            } else {
                // Parse to validate; normalize to UTC for cursor comparisons.
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|t| t.with_timezone(&Utc).to_rfc3339())
            }
        })
        .max()
}

/// Convenience wrapper over [`max_updated_at_refs`] for owned slices (used by tests).
#[cfg(test)]
fn max_updated_at(entries: &[Value]) -> Option<String> {
    let refs: Vec<&Value> = entries.iter().collect();
    max_updated_at_refs(&refs)
}

// ---------------------------------------------------------------------------
// Write: raw + contract (two-dedupe-key pattern from Toggl/Clockify).

fn write_entries(vault: &Vault, rows: Vec<(TimeEntry, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing contract ids -- first-observed wins on re-poll.
    let mut seen_ids: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let id = str_field(&v, "id");
            if !id.is_empty() {
                seen_ids.insert(id);
            }
        }
    }

    // Existing raw (id, updated_at) -- any new state snapshot is appended.
    let mut seen_raw: HashSet<(String, String)> = HashSet::new();
    for key in raw.partitions()? {
        for v in raw.read::<Value>(&key)? {
            if let Some(k) = raw_key(&v) {
                seen_raw.insert(k);
            }
        }
    }

    let mut new_rows: Vec<TimeEntry> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (row, raw_val) in rows {
        if let Some(k) = raw_key(&raw_val) {
            if seen_raw.insert(k) {
                new_raws.push(RawLine { ts: row.start.clone(), value: raw_val });
            }
        }
        if !row.id.is_empty() && seen_ids.insert(row.id.clone()) {
            new_rows.push(row);
        }
    }

    contract.append(&new_rows, |r| &r.start)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// Pull.

fn load_creds(vault: &Vault) -> Result<(String, String)> {
    let raw = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Harvest is not connected -- add your token and Account ID in the Integrations tab")?;
    parse_composite(&raw)
        .context("stored Harvest credential is malformed -- reconnect from the Integrations tab")
}

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let (token, account_id) = load_creds(vault)?;
    let client = HarvestClient::new(API_BASE.to_string(), token, account_id);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl HarvestApi) -> Result<PullOutcome> {
    let mut state = vault.read_harvest_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut all_entries: Vec<Value> = Vec::new();

    // Drain all pages before advancing the cursor -- a crash re-drains from
    // the same updated_since, so no entries are silently skipped.
    // Follow links.next verbatim: Harvest may be cursor-based, so we never
    // reconstruct ?page=N ourselves after the first request.
    let mut next_url: Option<String> = None;
    loop {
        let (entries, returned_next) = api
            .time_entries_page(state.updated_since.as_deref(), next_url.as_deref())
            .map_err(|e| fetch_err("time_entries", e))?;

        // Done when: empty page, or no next link (last/only page).
        let done = entries.is_empty() || returned_next.is_none();
        all_entries.extend(entries);
        if done {
            break;
        }
        next_url = returned_next;
    }

    // Map raw entries to contract rows; collect both together so the watermark
    // can be derived only over entries that were SUCCESSFULLY mapped.
    // Entries that entry_from() drops (missing id / spent_date) must NOT
    // advance the cursor — if a future API shape drift makes them unmappable
    // they would be silently lost on the next incremental poll.
    let rows: Vec<(TimeEntry, Value)> = all_entries
        .iter()
        .filter_map(|e| entry_from(e).map(|t| (t, e.clone())))
        .collect();

    // Watermark candidate: max updated_at across SUCCESSFULLY MAPPED entries only.
    let mapped_raws: Vec<&Value> = rows.iter().map(|(_, raw)| raw).collect();
    let at_watermark = max_updated_at_refs(&mapped_raws);

    let written = write_entries(vault, rows)?;
    counts.insert("entries", written);

    // Advance the watermark only after the full drain; only advance forward.
    if let Some(w) = at_watermark {
        let advance = state.updated_since.as_ref().map_or(true, |cur| w > *cur);
        if advance {
            state.updated_since = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_harvest_sync(&state)?;

    let e = counts.get("entries").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("{e} time entries"),
        counts,
    })
}

fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Harvest rejected the credentials (401) on {endpoint} -- reconnect from the \
             Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Harvest rate-limited the {endpoint} endpoint (429) -- will retry next sync"
        ),
        other => anyhow::anyhow!("Harvest {endpoint} fetch failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-harvest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (documented v2 /time_entries shape, confirmed against
    //     help.getharvest.com/api-v2/timesheets-api/timesheets/time-entries/) -

    /// A stopped time entry -- the canonical v2 shape with all fields.
    fn entry_stopped() -> Value {
        json!({
            "id": 636708723_i64,
            "spent_date": "2017-03-01",
            "user": {"id": 1782959, "name": "Kim Allen"},
            "client": {"id": 5735776, "name": "123 Industries"},
            "project": {"id": 14308069, "name": "Online Store - Phase 1"},
            "task": {"id": 8083366, "name": "Programming"},
            "user_assignment": {
                "id": 125068554, "is_project_manager": true, "is_active": true,
                "budget": null, "created_at": "2017-06-26T22:32:52Z",
                "updated_at": "2017-06-26T22:32:52Z", "hourly_rate": 100.0
            },
            "task_assignment": {
                "id": 155505014, "billable": true, "is_active": true,
                "created_at": "2017-06-26T21:52:18Z", "updated_at": "2017-06-26T21:52:18Z",
                "hourly_rate": 100.0, "budget": null
            },
            "hours": 1.35_f64,
            "hours_without_timer": 1.35_f64,
            "rounded_hours": 1.5_f64,
            "notes": "Importing products",
            "created_at": "2017-06-27T15:49:28Z",
            "updated_at": "2017-06-27T16:47:14Z",
            "is_locked": true,
            "locked_reason": "Item Invoiced and Approved",
            "is_closed": true,
            "approval_status": "approved",
            "is_billed": true,
            "timer_started_at": null,
            "started_time": "1:00pm",
            "ended_time": "2:00pm",
            "is_running": false,
            "invoice": {"id": 13150403, "number": "1001"},
            "external_reference": null,
            "billable": true,
            "budgeted": true,
            "billable_rate": 100.0_f64,
            "cost_rate": 50.0_f64
        })
    }

    /// A running timer: `is_running: true`, `timer_started_at` non-null.
    fn entry_running() -> Value {
        json!({
            "id": 636709000_i64,
            "spent_date": "2017-03-02",
            "user": {"id": 1782959, "name": "Kim Allen"},
            "client": {"id": 5735776, "name": "123 Industries"},
            "project": {"id": 14308069, "name": "Online Store - Phase 1"},
            "task": {"id": 8083367, "name": "Design"},
            "hours": 0.0_f64,
            "hours_without_timer": 0.0_f64,
            "rounded_hours": 0.0_f64,
            "notes": "Wireframing",
            "created_at": "2017-03-02T14:00:00Z",
            "updated_at": "2017-03-02T14:00:01Z",
            "is_locked": false,
            "locked_reason": null,
            "is_closed": false,
            "approval_status": "unsubmitted",
            "is_billed": false,
            "timer_started_at": "2017-03-02T14:00:00Z",
            "started_time": "2:00pm",
            "ended_time": null,
            "is_running": true,
            "invoice": null,
            "external_reference": null,
            "billable": false,
            "budgeted": false,
            "billable_rate": null,
            "cost_rate": null
        })
    }

    /// A different-month entry to test partition splitting.
    fn entry_may() -> Value {
        json!({
            "id": 636710000_i64,
            "spent_date": "2017-05-10",
            "user": {"id": 1782959, "name": "Kim Allen"},
            "client": {"id": 5735776, "name": "Acme Co"},
            "project": {"id": 14308070, "name": "Mobile App"},
            "task": {"id": 8083370, "name": "Backend"},
            "hours": 3.0_f64,
            "hours_without_timer": 3.0_f64,
            "rounded_hours": 3.0_f64,
            "notes": "API integration",
            "created_at": "2017-05-10T10:00:00Z",
            "updated_at": "2017-05-10T13:00:00Z",
            "is_locked": false,
            "locked_reason": null,
            "is_closed": false,
            "approval_status": "unsubmitted",
            "is_billed": false,
            "timer_started_at": null,
            "started_time": "10:00am",
            "ended_time": "1:00pm",
            "is_running": false,
            "invoice": null,
            "external_reference": null,
            "billable": true,
            "budgeted": true,
            "billable_rate": 125.0_f64,
            "cost_rate": null
        })
    }

    // --- pure mapping tests -------------------------------------------------

    #[test]
    fn maps_stopped_entry_all_fields() {
        let e = entry_from(&entry_stopped()).unwrap();
        assert_eq!(e.source, "harvest");
        assert_eq!(e.id, "636708723");
        assert_eq!(e.start, "2017-03-01", "date-only start from spent_date");
        assert!(e.end.is_empty(), "no end timestamp (date-only format)");
        // 1.35 * 3600 = 4860.0 exactly -> 4860
        assert_eq!(e.duration_secs, Some(4860), "1.35h -> 4860s");
        assert_eq!(e.description, "Importing products");
        assert_eq!(e.project, "Online Store - Phase 1");
        assert_eq!(e.client, "123 Industries");
        assert_eq!(e.task, "Programming");
        assert!(e.tags.is_empty(), "Harvest v2 has no tag field on entries");
        assert_eq!(e.billable, Some(true));
        assert_eq!(e.extra.get("project_id"), Some(&json!(14308069_i64)));
        assert_eq!(e.extra.get("client_id"),  Some(&json!(5735776_i64)));
        assert_eq!(e.extra.get("task_id"),    Some(&json!(8083366_i64)));
        assert_eq!(e.extra.get("user_id"),    Some(&json!(1782959_i64)));
        assert_eq!(e.extra.get("hours"),      Some(&json!(1.35_f64)));
        assert_eq!(e.extra.get("invoice_number"), Some(&json!("1001")));
        assert_eq!(e.extra.get("invoice_id"), Some(&json!(13150403_i64)));
        assert_eq!(e.extra.get("is_billed"),  Some(&json!(true)));
        assert_eq!(e.extra.get("is_locked"),  Some(&json!(true)));
        assert_eq!(e.extra.get("approval_status"), Some(&json!("approved")));
        assert_eq!(e.extra.get("started_time"), Some(&json!("1:00pm")));
        assert_eq!(e.extra.get("ended_time"),   Some(&json!("2:00pm")));
    }

    #[test]
    fn running_timer_omits_duration_secs() {
        let e = entry_from(&entry_running()).unwrap();
        assert_eq!(e.id, "636709000");
        assert!(e.duration_secs.is_none(), "running timer: no duration_secs");
        assert!(e.end.is_empty());
        assert_eq!(e.description, "Wireframing");
        assert_eq!(e.project, "Online Store - Phase 1");
        assert_eq!(e.client, "123 Industries");
        assert_eq!(e.billable, Some(false));
        assert!(e.extra.contains_key("timer_started_at"), "timer_started_at in extra");
        let re = serde_json::to_value(&e).unwrap();
        assert!(re.get("end").is_none());
        assert!(re.get("duration_secs").is_none());
    }

    #[test]
    fn hours_conversion_rounds_correctly() {
        assert_eq!(hours_to_secs(1.35), 4860);
        assert_eq!(hours_to_secs(1.5),  5400);
        assert_eq!(hours_to_secs(0.25), 900);
        assert_eq!(hours_to_secs(3.0),  10800);
    }

    #[test]
    fn entry_without_spent_date_returns_none() {
        let mut bad = entry_stopped();
        bad.as_object_mut().unwrap().remove("spent_date");
        assert!(entry_from(&bad).is_none());
    }

    #[test]
    fn entry_without_id_returns_none() {
        let mut bad = entry_stopped();
        bad.as_object_mut().unwrap().remove("id");
        assert!(entry_from(&bad).is_none());
    }

    #[test]
    fn max_updated_at_picks_latest() {
        let entries = vec![entry_stopped(), entry_running(), entry_may()];
        // entry_stopped.updated_at = "2017-06-27T16:47:14Z" is the latest
        // (later than running's 2017-03-02T14:00:01Z and may's 2017-05-10T13:00:00Z).
        let m = max_updated_at(&entries).unwrap();
        assert!(m.contains("2017-06-27"), "max is the stopped entry: {m}");
    }

    #[test]
    fn parse_composite_splits_pipe_and_newline() {
        let (t, a) = parse_composite("MyToken|12345").unwrap();
        assert_eq!(t, "MyToken");
        assert_eq!(a, "12345");

        let (t2, a2) = parse_composite("  MyToken \n 12345  ").unwrap();
        assert_eq!(t2, "MyToken");
        assert_eq!(a2, "12345");

        assert!(parse_composite("OnlyToken").is_none(), "no separator");
        assert!(parse_composite("|12345").is_none(), "empty PAT");
        assert!(parse_composite("MyToken|").is_none(), "empty account_id");
    }

    // --- mock API -----------------------------------------------------------

    struct MockApi {
        pages: RefCell<Vec<(Vec<Value>, Option<String>)>>,
        since_seen: RefCell<Vec<Option<String>>>,
        /// next_url values received on each call (None on first call, Some(url) thereafter).
        next_urls_seen: RefCell<Vec<Option<String>>>,
    }

    impl MockApi {
        fn one_page(entries: Vec<Value>) -> Self {
            MockApi {
                pages: RefCell::new(vec![(entries, None)]),
                since_seen: RefCell::new(Vec::new()),
                next_urls_seen: RefCell::new(Vec::new()),
            }
        }
        /// Two-page scenario: pop order is reversed so first page is returned
        /// first. First page carries a `next` URL; second page has `None`.
        fn two_pages(first: Vec<Value>, second: Vec<Value>) -> Self {
            MockApi {
                pages: RefCell::new(vec![
                    (second, None),
                    (first, Some("https://api.harvestapp.com/v2/time_entries?cursor=abc123".into())),
                ]),
                since_seen: RefCell::new(Vec::new()),
                next_urls_seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl HarvestApi for MockApi {
        fn time_entries_page(
            &self,
            updated_since: Option<&str>,
            next_url: Option<&str>,
        ) -> Result<(Vec<Value>, Option<String>), FetchError> {
            self.since_seen.borrow_mut().push(updated_since.map(str::to_string));
            self.next_urls_seen.borrow_mut().push(next_url.map(str::to_string));
            Ok(self.pages.borrow_mut().pop().unwrap_or_else(|| (Vec::new(), None)))
        }
    }

    struct ErrApi;
    impl HarvestApi for ErrApi {
        fn time_entries_page(
            &self,
            _since: Option<&str>,
            _next_url: Option<&str>,
        ) -> Result<(Vec<Value>, Option<String>), FetchError> {
            Err(FetchError::Other("boom".into()))
        }
    }

    #[test]
    fn full_pull_writes_contract_and_raw_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::one_page(vec![entry_stopped(), entry_running()]);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&2));

        // Contract rows partitioned by spent_date month.
        let mar = std::fs::read_to_string(
            v.root().join("time-entries/harvest/2017-03.jsonl"),
        )
        .unwrap();
        assert_eq!(mar.lines().count(), 2, "both March entries");
        assert!(mar.contains("\"id\":\"636708723\""));
        assert!(mar.contains("\"id\":\"636709000\""));
        assert!(mar.contains("\"project\":\"Online Store - Phase 1\""));
        assert!(mar.contains("\"client\":\"123 Industries\""));
        assert!(mar.contains("\"duration_secs\":4860"), "stopped has duration");
        // Running entry must NOT have duration_secs.
        let running_line = mar.lines().find(|l| l.contains("636709000")).unwrap();
        assert!(!running_line.contains("duration_secs"), "running: no duration_secs in contract");

        // Raw layer keeps verbatim API objects.
        let raw_mar = std::fs::read_to_string(
            v.root().join("time-entries/harvest/raw/2017-03.jsonl"),
        )
        .unwrap();
        assert!(raw_mar.contains("\"is_running\":true"), "raw keeps is_running");
        assert!(raw_mar.contains("\"hours\":1.35"), "raw keeps hours decimal");

        // Watermark is max updated_at across the drain
        // (running entry: 2017-03-02T14:00:01Z is later than stopped: 2017-06-27T...).
        // Actually stopped's updated_at is 2017-06-27T16:47:14Z (the latest!).
        let state = v.read_harvest_sync();
        assert!(state.updated_since.is_some(), "cursor set after sync");
        let ws = state.updated_since.as_deref().unwrap();
        assert!(ws.contains("2017-06-27"), "watermark is the stopped entry's updated_at: {ws}");
        assert!(state.updated.is_some());

        // Cursor file must not contain the token.
        let cursor = std::fs::read_to_string(v.root().join(".trove/harvest-sync.json")).unwrap();
        assert!(!cursor.contains("Bearer") && !cursor.contains("access_token"));

        // Re-run with same data: id dedupe -> zero new contract rows.
        let api2 = MockApi::one_page(vec![entry_stopped(), entry_running()]);
        let again = pull_with(&v, &api2).unwrap();
        assert_eq!(again.counts.get("entries"), Some(&0), "re-sync: no new ids");
        // Re-sync passes the stored watermark as updated_since.
        let since = api2.since_seen.borrow();
        assert!(
            since[0].as_deref().map(|s| s.contains("2017-06-27")).unwrap_or(false),
            "watermark passed on re-sync"
        );
    }

    #[test]
    fn multi_month_entries_partition_correctly() {
        let v = temp_vault("multimonth");
        let api = MockApi::one_page(vec![entry_stopped(), entry_may()]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&2));
        assert!(v.root().join("time-entries/harvest/2017-03.jsonl").exists());
        assert!(v.root().join("time-entries/harvest/2017-05.jsonl").exists());
    }

    #[test]
    fn two_page_drain_collects_all() {
        let v = temp_vault("twopages");
        let api = MockApi::two_pages(vec![entry_stopped()], vec![entry_may()]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&2), "both pages drained");
    }

    #[test]
    fn pagination_follows_next_url_verbatim() {
        // Defect 3/4: the loop must pass links.next back to the API verbatim on
        // the second call rather than re-constructing ?page=N.
        let v = temp_vault("urlfollow");
        let cursor_url = "https://api.harvestapp.com/v2/time_entries?cursor=abc123";
        let api = MockApi::two_pages(vec![entry_stopped()], vec![entry_may()]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&2), "both pages drained");
        // First call: next_url is None (initial request).
        let urls = api.next_urls_seen.borrow();
        assert_eq!(urls.len(), 2, "exactly two API calls");
        assert!(urls[0].is_none(), "first call sends no next_url");
        // Second call: next_url equals the cursor URL returned by the first page.
        assert_eq!(
            urls[1].as_deref(),
            Some(cursor_url),
            "second call follows links.next verbatim: {:?}",
            urls[1]
        );
    }

    #[test]
    fn stopped_zero_duration_emits_some_zero() {
        // Defect 2: a stopped entry with hours==0.0 must NOT be serialized
        // identically to a running timer.
        let mut e = entry_running();
        // Flip is_running off to simulate a just-stopped or manually-created
        // zero-duration entry.
        e.as_object_mut().unwrap().insert("is_running".into(), serde_json::Value::Bool(false));
        let mapped = entry_from(&e).unwrap();
        // Must be Some(0), not None.
        assert_eq!(
            mapped.duration_secs,
            Some(0),
            "stopped 0h entry -> duration_secs: Some(0), not None"
        );
    }

    #[test]
    fn watermark_not_advanced_by_parse_miss() {
        // Defect 1: if entry_from() fails for some entries, the cursor must
        // not advance past the successfully-mapped ones only.
        let v = temp_vault("parsemiss");

        // Two entries: one valid, one with a missing `spent_date` (parse miss).
        let mut bad = entry_may();
        bad.as_object_mut().unwrap().remove("spent_date");
        // Make the bad entry's updated_at LATER than the good one so that
        // advancing from all_entries would produce a higher watermark.
        bad.as_object_mut().unwrap().insert(
            "updated_at".into(),
            serde_json::Value::String("2025-01-01T00:00:00Z".into()),
        );
        let api = MockApi::one_page(vec![entry_stopped(), bad]);
        let out = pull_with(&v, &api).unwrap();

        // Only the valid entry was written.
        assert_eq!(out.counts.get("entries"), Some(&1), "only valid entry written");

        // Watermark must come from the VALID entry (2017-06-27...) not from
        // the bad entry's later updated_at (2025-01-01...).
        let ws = v.read_harvest_sync().updated_since.unwrap();
        assert!(
            ws.contains("2017-06-27"),
            "cursor derived from valid entry only, not from parse-miss: {ws}"
        );
    }

    #[test]
    fn fetch_error_does_not_advance_watermark() {
        let v = temp_vault("fetcherr");
        v.write_harvest_sync(&SyncState {
            updated_since: Some("2017-01-01T00:00:00+00:00".into()),
            updated: None,
        })
        .unwrap();
        let err = pull_with(&v, &ErrApi).unwrap_err().to_string();
        assert!(err.contains("time_entries"), "error names endpoint: {err}");
        assert_eq!(
            v.read_harvest_sync().updated_since.as_deref(),
            Some("2017-01-01T00:00:00+00:00"),
            "watermark unchanged after failed drain"
        );
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.updated_since.is_none());
        assert!(empty.updated.is_none());

        let partial: SyncState =
            serde_json::from_str(r#"{"updated_since":"2017-06-01T00:00:00+00:00"}"#).unwrap();
        assert_eq!(
            partial.updated_since.as_deref(),
            Some("2017-06-01T00:00:00+00:00")
        );
        assert!(partial.updated.is_none());
    }

    #[test]
    fn connection_stores_composite_and_status_reflects_it() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "harvest_tok|9876543".into(),
                refresh_token: None,
                token_type: Some("Bearer".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "harvest");
        assert_eq!(status.accounts[0].label, "Harvest");

        // Token NOT in the cursor file.
        v.write_harvest_sync(&SyncState {
            updated_since: Some("2017-01-01T00:00:00Z".into()),
            updated: Some("2017-06-15T00:00:00-07:00".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/harvest-sync.json")).unwrap();
        assert!(!cursor.contains("harvest_tok"), "PAT never in cursor file");

        def_disconnect(&v, "harvest").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn not_connected_gives_clear_error() {
        let v = temp_vault("noconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear message: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "harvest");
        assert_eq!(DEF.connection, Some("harvest"));
    }
}
