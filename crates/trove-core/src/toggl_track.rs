//! Toggl Track — one of the most popular manual time trackers. A **Periodic**
//! cloud pull of the user's time entries into the bound [`crate::time_entries`]
//! contract (**first collector in the `time-entries` domain** — this build
//! binds the contract; see `crate::time_entries` / `crate::contracts`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/toggl-track.md.
//!
//! Two endpoints, one account token (HTTP Basic, `Authorization: Basic
//! base64(<token>:api_token)` — the documented Toggl scheme):
//!
//! - `GET /api/v9/me/projects` → projects (`id` → `name`), so an entry's
//!   `project_id` resolves to the human project name the contract carries.
//! - `GET /api/v9/me/time_entries` → time entries. Each becomes a
//!   [`crate::time_entries::TimeEntry`] under
//!   `time-entries/toggl-track/YYYY-MM.jsonl` (`id` = the entry id, the dedupe
//!   key; `start` = the source UTC start → **local**, the partition key;
//!   `stop` → `end` (omitted while a timer runs); `duration` → `duration_secs`
//!   (omitted, like `end`, while running — a running entry's `duration` is
//!   negative); `project_id` → `project` name; `tags` verbatim; `billable`).
//!   Source-specific bits (`workspace_id`, `task_id`, `user_id`, `tag_ids`,
//!   `at`, `duronly`, …) ride in `extra`.
//!
//! Two layers per pull, with two different dedupe keys (see `write_entries`):
//! the **raw** API object verbatim under
//! `time-entries/toggl-track/raw/YYYY-MM.jsonl` (full fidelity — every distinct
//! state is kept, deduped only by `id`+`at` so an unchanged re-poll doesn't
//! duplicate but a running→stopped re-emit IS recorded), and the normalized
//! **contract** rows, append-only and deduped by `id` (first observed state of
//! an id wins). So an entry first seen running keeps its open contract row,
//! while its final end/duration are preserved in the raw layer.
//!
//! ## Incremental — the `since` watermark
//!
//! `/me/time_entries` takes a `since` (Unix epoch seconds) lower bound that
//! returns entries created/updated/deleted since then; we persist a watermark
//! (the max `at` last-modified across the drain, as epoch seconds) in
//! `.trove/toggl-track-sync.json` (non-secret, rebuildable) and only advance it
//! after a full drain, so a crash re-drains rather than skips. A re-fetched
//! boundary entry is harmless — the `id` dedupe absorbs it. The **first** sync
//! has no cursor and omits `since`, taking Toggl's default recent window (the
//! API returns roughly the last three months unbounded — deep backfill would
//! need date-range paging against the 30 req/hour `/me` cap, so the lean
//! incremental pull is the default and the caveat documents the limit).
//!
//! A soft-deleted entry (non-null `server_deleted_at`) still moves the
//! watermark (so a re-poll doesn't re-fetch it) but is **not** written as an
//! active contract row — a tombstone is not a logged entry.
//!
//! Auth is a secret: the API token is pasted via
//! [`ConnectMethod::TokenPaste`], stored under `.trove/sync/` (0600), verified
//! at connect with a real `GET /api/v9/me`, and never logged or written to the
//! cursor or any non-secret file.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Local};
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

/// Contract-layer entry stream; raw under `raw/`.
const DIR: &str = "time-entries/toggl-track";
const RAW_DIR: &str = "time-entries/toggl-track/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it just re-asks the default recent window again.
const SYNC_FILE: &str = ".trove/toggl-track-sync.json";

/// Service id under `.trove/sync/` where the pasted token is stored (the
/// GitHub/Todoist-PAT slot: the token rides a never-expiring [`TokenSet`]).
const SERVICE: &str = "toggl-track";

const API_BASE: &str = "https://api.track.toggl.com";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between syncs in the watcher loop. Hourly: entries trickle in, the
/// incremental `since` poll is cheap, and the `/me` API caps at 30 req/hour —
/// an hourly two-call pull stays comfortably under budget.
pub const TOGGL_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing token or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!("toggl-track synced — {} time entries", c("entries"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "toggl-track sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Toggl Track synced — {} time entries", c("entries")),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "toggl-track",
        name: "Toggl Track",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Toggl Track time entries into the unified time-entries store \
                      via the official API (api.track.toggl.com/api/v9), every hour. First sync \
                      backfills the recent window; later syncs fetch only what changed.",
        domain: "time-entries",
        vault_path: "time-entries/toggl-track/",
        toggleable: true,
        setup: &[
            "Connect with your Toggl Track API token on this card.",
            "First sync backfills your recent time entries; later syncs are incremental.",
        ],
        caveats: "The /me API is rate-limited to 30 requests/hour, so syncs are hourly and a \
                  first sync backfills only the recent window the API returns by default (roughly \
                  the last few months) — older history isn't deep-backfilled. Project names are \
                  resolved from your project list; a since-deleted project leaves the entry's \
                  project blank. A project's client id is kept (under extra), but the client name \
                  isn't resolved — that needs a separate clients call we skip to stay under the \
                  rate limit. An entry first synced mid-timer keeps its running (open) row; its \
                  final duration lands in the raw layer once it stops.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(TOGGL_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("toggl-track"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a Toggl API token, a SECRET).

/// Verify the pasted token with `GET /api/v9/me`, then store it (0600). A 401
/// bails with a clear message; the token is never logged.
fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Toggl Track API token from track.toggl.com/profile");
    }
    let client = TogglClient::new(API_BASE.to_string(), token.to_string());
    match client.verify() {
        Ok(()) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Toggl Track rejected the token (401) — copy it fresh from the bottom of \
             track.toggl.com/profile"
        ),
        Err(e) => bail!("Toggl Track auth check failed: {e}"),
    }
    // The token goes ONLY through the secret store (0600). Never the cursor.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: Some("Basic".into()),
            scope: None,
            expires_at: None,
        },
    )
}

/// Forget the stored token. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = the token is stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Toggl Track".to_string(),
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // the API token doesn't expire
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: a personal API token is self-service.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste the
/// API token (used as the HTTP Basic username with the literal `api_token`
/// password).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "toggl-track",
    display_name: "Toggl Track",
    methods: &[ConnectMethod::TokenPaste {
        label: "Toggl Track API token",
        help: "Paste your API token from the bottom of track.toggl.com/profile — it's stored \
               locally and never sent anywhere but Toggl.",
        placeholder: "1234abcd5678ef90…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["toggl-track"],
    setup: &[
        "Open track.toggl.com/profile while signed in to Toggl Track.",
        "Scroll to the bottom and copy your API token.",
        "Paste it here — it's stored locally and used only to reach Toggl.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors: 401 wants distinct handling (clear reconnect),
/// 429 is the rate-limit (transient — the watcher's hourly cadence keeps us
/// under the 30/hour `/me` budget), everything else is a message.
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

/// The endpoints the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network.
trait TogglApi {
    /// `GET /api/v9/me/projects` → the full project list (one un-paged array).
    fn projects(&self) -> Result<Vec<Value>, FetchError>;

    /// `GET /api/v9/me/time_entries` → time entries. `since`, when set, is the
    /// Unix-epoch-seconds lower bound (created/updated/deleted since then).
    fn time_entries(&self, since: Option<i64>) -> Result<Vec<Value>, FetchError>;
}

/// Thin client; base URL injected (the github/oura/lastfm/todoist pattern).
struct TogglClient {
    base: String,
    token: String,
}

impl TogglClient {
    fn new(base: String, token: String) -> Self {
        TogglClient { base, token }
    }

    /// The HTTP Basic header: `base64(<token>:api_token)` — Toggl's documented
    /// scheme (the API token is the username, the literal `api_token` the
    /// password).
    fn auth_header(&self) -> String {
        format!("Basic {}", STANDARD.encode(format!("{}:api_token", self.token)))
    }

    /// `GET /api/v9/me` → 200 when the token is valid. Used at connect.
    fn verify(&self) -> Result<(), FetchError> {
        let url = format!("{}/api/v9/me", self.base);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &self.auth_header())
            .call()
        {
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

    /// Shared GET → a JSON array, read defensively (a non-array body yields an
    /// empty vec rather than an error — leniency by convention).
    fn get_array(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<Value>, FetchError> {
        let mut req = ureq::get(&format!("{}{path}", self.base))
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &self.auth_header());
        for (k, v) in query {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(match v {
                    Value::Array(a) => a,
                    _ => Vec::new(),
                })
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

impl TogglApi for TogglClient {
    fn projects(&self) -> Result<Vec<Value>, FetchError> {
        self.get_array("/api/v9/me/projects", &[])
    }

    fn time_entries(&self, since: Option<i64>) -> Result<Vec<Value>, FetchError> {
        let query: Vec<(&str, String)> = match since {
            Some(s) => vec![("since", s.to_string())],
            None => Vec::new(),
        };
        self.get_array("/api/v9/me/time_entries", &query)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max entry `at` (last-modified) seen, as Unix epoch **seconds** — the
    /// `since` lower bound for the next `/me/time_entries` poll. `None` on a
    /// first sync (omit `since`, take the default recent window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    since: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_toggl_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_toggl_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity API object). The on-disk line is the verbatim
// API object (flattened — no synthetic columns), tagged with the contract ts
// purely so the month-partition writer files it under the right month. Only
// `value` is serialized.

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
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// An id field that may be a JSON number or string → a `String`. Toggl ids are
/// integers; accept a string defensively.
fn value_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// An RFC3339-ish timestamp → RFC3339 local. Unparseable/empty values pass
/// through verbatim rather than being dropped (the github/todoist idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// An RFC3339-ish timestamp → Unix epoch **seconds**, for the `since`
/// watermark. `None` when unparseable.
fn to_epoch(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp())
}

/// Insert `k`→`v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// True when the entry is a soft-delete tombstone (non-null
/// `server_deleted_at`). Those move the watermark but are not active rows.
fn is_deleted(e: &Value) -> bool {
    e.get("server_deleted_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
        && !str_field(e, "server_deleted_at").is_empty()
}

/// A v9 time-entry object → a contract [`TimeEntry`], resolving the project
/// NAME from `project_names` (project_id→name). `project_clients`
/// (project_id→client_id) lets us preserve the entry's client linkage in
/// `extra`: the v9 time-entry object carries no client field, but the project
/// object (already fetched for the name map) carries `client_id`, so we stash
/// it rather than dropping it. (The client *name* would need a separate
/// `/me/clients` call against the 30/hr `/me` cap, so the contract's `client`
/// column is intentionally left blank — see the brief caveats; the id keeps the
/// linkage for a read-time join.) `None` when the entry has no id (can't dedup)
/// or no usable `start` that yields a month partition (can't be filed).
fn entry_from(
    e: &Value,
    project_names: &HashMap<String, String>,
    project_clients: &HashMap<String, i64>,
) -> Option<TimeEntry> {
    let id = e.get("id").and_then(value_id)?;
    let raw_start = str_field(e, "start");
    if raw_start.is_empty() {
        return None;
    }
    let start = to_local(&raw_start);
    // Must yield a month partition; otherwise the row can't be filed.
    Partition::Month.key(&start)?;

    // A running timer has a negative `duration` and no `stop`: omit both `end`
    // and `duration_secs`. A stopped entry has duration >= 0 and a `stop`.
    let duration_raw = e.get("duration").and_then(Value::as_i64);
    let running = duration_raw.is_some_and(|d| d < 0);
    let stop = str_field(e, "stop");
    let end = if running || stop.is_empty() {
        String::new()
    } else {
        to_local(&stop)
    };
    let duration_secs = match duration_raw {
        Some(d) if d >= 0 && !running => Some(d),
        _ => None,
    };

    // project_id → project name (resolved from the project list). The raw id
    // is preserved in extra regardless.
    let project_id = e.get("project_id").and_then(value_id);
    let project = project_id
        .as_ref()
        .and_then(|pid| project_names.get(pid))
        .cloned()
        .unwrap_or_default();

    let tags: Vec<String> = e
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let billable = e.get("billable").and_then(Value::as_bool);

    // Source-specific bits ride in extra — full fidelity beyond the contract
    // columns (the raw layer keeps everything regardless).
    let mut extra = Map::new();
    if let Some(wid) = e.get("workspace_id").and_then(Value::as_i64) {
        extra.insert("workspace_id".into(), Value::from(wid));
    }
    if let Some(pid) = &project_id {
        // Numeric where possible (the source shape), else the string form.
        match pid.parse::<i64>() {
            Ok(n) => extra.insert("project_id".into(), Value::from(n)),
            Err(_) => extra.insert("project_id".into(), Value::from(pid.clone())),
        };
        // The entry's client linkage, recovered from the project (the entry
        // itself carries none). Kept as the id so it survives without the extra
        // `/me/clients` call needed to resolve a name.
        if let Some(cid) = project_clients.get(pid) {
            extra.insert("client_id".into(), Value::from(*cid));
        }
    }
    if let Some(tid) = e.get("task_id").and_then(Value::as_i64) {
        extra.insert("task_id".into(), Value::from(tid));
    }
    if let Some(uid) = e.get("user_id").and_then(Value::as_i64) {
        extra.insert("user_id".into(), Value::from(uid));
    }
    if let Some(tag_ids) = e.get("tag_ids").and_then(Value::as_array) {
        if !tag_ids.is_empty() {
            extra.insert("tag_ids".into(), Value::Array(tag_ids.clone()));
        }
    }
    if e.get("duronly").and_then(Value::as_bool) == Some(true) {
        extra.insert("duronly".into(), Value::Bool(true));
    }
    // `at` (last-modified) in local time, useful for cross-referencing.
    let at = str_field(e, "at");
    if !at.is_empty() {
        put_str(&mut extra, "at", &to_local(&at));
    }

    Some(TimeEntry {
        source: "toggl-track".into(),
        id,
        start,
        end,
        duration_secs,
        description: str_field(e, "description"),
        project,
        // The contract `client` is the client *name*, which v9 exposes only via
        // a separate `/me/clients` call (skipped — the 30/hr `/me` budget). The
        // client *id* is preserved in `extra` (above) so the linkage isn't lost.
        client: String::new(),
        task: String::new(), // task is an id (task_id → extra), not a name here
        tags,
        billable,
        extra,
    })
}

/// Max entry `at` across a slice, as Unix epoch seconds — the next `since`.
/// `None` when no entry has a parseable `at`.
fn max_at_epoch(entries: &[Value]) -> Option<i64> {
    entries
        .iter()
        .filter_map(|e| {
            let at = str_field(e, "at");
            (!at.is_empty()).then(|| to_epoch(&at)).flatten()
        })
        .max()
}

/// One `/me/projects` item → (id, name). `None` without both.
fn project_id_name(v: &Value) -> Option<(String, String)> {
    let id = v.get("id").and_then(value_id)?;
    let name = str_field(v, "name");
    (!name.is_empty()).then_some((id, name))
}

/// One `/me/projects` item → (project_id, client_id). `None` when the project
/// has no id or its `client_id` is absent/null (an unclient'd project) — so the
/// linkage is only recorded where it exists. (v9 projects carry `client_id` but
/// no client name, so only the id is recoverable without a `/me/clients` call.)
fn project_id_client(v: &Value) -> Option<(String, i64)> {
    let id = v.get("id").and_then(value_id)?;
    let client_id = v.get("client_id").and_then(Value::as_i64)?;
    Some((id, client_id))
}

// ---------------------------------------------------------------------------
// Write: raw + contract.
//
// Two different dedupe keys, deliberately:
//   - The **contract** stream is append-only (ContractKind::EventStream) and
//     dedupes by `id` alone: the first observed state of an id wins (an entry
//     first seen running stays open; its later stopped re-emit is not
//     re-appended). The first-observed open row is what readers see.
//   - The **raw** stream is full-fidelity and dedupes by (`id`, `at`) — the
//     entry's last-modified stamp. A re-fetched entry whose state changed has a
//     newer `at`, so its verbatim snapshot IS appended (the running→stopped
//     re-emit lands here, so the final end/duration are never lost), while a
//     byte-for-byte identical re-poll (same id, same at) still dedupes, keeping
//     re-runs idempotent.

/// The raw-layer identity of a fetched object: (`id`, `at`). `at` advances each
/// time Toggl mutates the entry, so distinct states get distinct keys while an
/// unchanged re-poll collapses. Returns `None` without an id (can't key).
fn raw_key(v: &Value) -> Option<(String, String)> {
    let id = v.get("id").and_then(value_id)?;
    Some((id, str_field(v, "at")))
}

/// Append new contract + raw rows. Returns the number of new **contract** rows
/// written. Raw lines partition by the same month as their contract row.
fn write_entries(vault: &Vault, rows: Vec<(TimeEntry, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing contract ids — re-runnable: a re-pull of an overlapping window
    // never duplicates a contract row (the letterboxd/readwise pattern).
    let mut seen_ids: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let id = str_field(&v, "id");
            if !id.is_empty() {
                seen_ids.insert(id);
            }
        }
    }

    // Existing raw (id, at) pairs — raw keeps every distinct state but still
    // dedupes an identical re-poll, so re-runs stay byte-identical.
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
        // Raw: append a verbatim snapshot for every state not already on disk
        // (keyed by id+at), independent of the contract dedupe.
        if let Some(k) = raw_key(&raw_val) {
            if seen_raw.insert(k) {
                new_raws.push(RawLine { ts: row.start.clone(), value: raw_val });
            }
        }
        // Contract: first-observed-by-id wins; later states are not re-appended.
        if !row.id.is_empty() && seen_ids.insert(row.id.clone()) {
            new_rows.push(row);
        }
    }

    contract.append(&new_rows, |r| &r.start)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the token and sync. Missing token ⇒ a quiet skip on the periodic
/// path (mirror todoist/readwise), a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Toggl Track is not connected — add your API token in the Integrations tab")?;
    let client = TogglClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl TogglApi) -> Result<PullOutcome> {
    let mut state = vault.read_toggl_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- projects: id→name and id→client_id --------------------------------
    // One fetch serves both maps: the name resolves the contract `project`
    // column, the client_id preserves the entry→client linkage in `extra` (the
    // entry object carries no client; the project does). No extra request.
    let project_items = api.projects().map_err(|e| fetch_err("projects", e))?;
    let project_names: HashMap<String, String> =
        project_items.iter().filter_map(project_id_name).collect();
    let project_clients: HashMap<String, i64> =
        project_items.iter().filter_map(project_id_client).collect();

    // --- time entries: raw + contract, deduped by id ----------------------
    // The `since` watermark drives the incremental window. We read the whole
    // returned set (Toggl returns one array, not a cursor-paged stream for the
    // `/me` window) before advancing the watermark, so a fetch error never
    // moves it — a crash re-drains.
    let entries = api
        .time_entries(state.since)
        .map_err(|e| fetch_err("time_entries", e))?;

    // Watermark candidate: the max `at` across the whole drain (incl.
    // tombstones we don't store), so a re-poll never re-fetches them either.
    let at_watermark = max_at_epoch(&entries);

    // Active entries only → contract rows (a soft-deleted tombstone is not a
    // logged entry, though it still moved the watermark above).
    let rows: Vec<(TimeEntry, Value)> = entries
        .iter()
        .filter(|e| !is_deleted(e))
        .filter_map(|e| entry_from(e, &project_names, &project_clients).map(|t| (t, e.clone())))
        .collect();
    let written = write_entries(vault, rows)?;
    counts.insert("entries", written);

    // Advance the watermark only after the full drain, and only forward.
    if let Some(w) = at_watermark {
        if state.since.is_none_or(|cur| w > cur) {
            state.since = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_toggl_sync(&state)?;

    let e = counts.get("entries").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("{e} time entries"),
        counts,
    })
}

/// Map a [`FetchError`] at the top of an endpoint into an anyhow error with a
/// clear reconnect message for 401.
fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Toggl Track rejected the token (401) on the {endpoint} endpoint — reconnect from the \
             Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Toggl Track rate limited the {endpoint} endpoint (429) — it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Toggl Track {endpoint} fetch failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-toggl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (the documented v9 /me/time_entries + /me/projects shapes) -

    /// A stopped time entry, exactly the v9 response shape (confirmed against
    /// the Toggl Track API v9 reference): `project_id`/`workspace_id`/`task_id`/
    /// `user_id`/`tag_ids` (not the legacy pid/wid/tid), UTC `start`/`stop`,
    /// positive `duration`.
    fn entry_stopped() -> Value {
        json!({
            "id": 3691827456_i64,
            "workspace_id": 1234567,
            "project_id": 9988776,
            "task_id": null,
            "user_id": 42,
            "billable": true,
            "start": "2026-06-10T16:00:00+00:00",
            "stop": "2026-06-10T17:30:00+00:00",
            "duration": 5400,
            "description": "Quarterly traffic report",
            "tags": ["deep-work"],
            "tag_ids": [555],
            "duronly": false,
            "at": "2026-06-10T17:30:05+00:00",
            "server_deleted_at": null
        })
    }

    /// A running timer: negative `duration` (start-of-entry as a negative
    /// epoch, per the v9 docs "negative, preferably -1") and no `stop`.
    fn entry_running() -> Value {
        json!({
            "id": 3691900001_i64,
            "workspace_id": 1234567,
            "project_id": 9988776,
            "user_id": 42,
            "billable": false,
            "start": "2026-06-15T15:00:00+00:00",
            "stop": null,
            "duration": -1,
            "description": "Writing the collector",
            "tags": [],
            "tag_ids": [],
            "at": "2026-06-15T15:00:01+00:00",
            "server_deleted_at": null
        })
    }

    /// A soft-deleted tombstone: non-null `server_deleted_at`. Must NOT become
    /// an active contract row, but still moves the watermark.
    fn entry_deleted() -> Value {
        json!({
            "id": 3690000000_i64,
            "workspace_id": 1234567,
            "project_id": 9988776,
            "user_id": 42,
            "billable": false,
            "start": "2026-06-09T12:00:00+00:00",
            "stop": "2026-06-09T12:30:00+00:00",
            "duration": 1800,
            "description": "Mistake entry",
            "tags": [],
            "at": "2026-06-12T09:00:00+00:00",
            "server_deleted_at": "2026-06-12T09:00:00+00:00"
        })
    }

    /// A v9 project object. `client_id` is present (integer, nullable) per the
    /// v9 schema — the entry→client linkage we recover into `extra` (the entry
    /// object itself carries no client). v9 carries no `client_name`.
    fn project_json(id: i64, name: &str) -> Value {
        json!({
            "id": id,
            "workspace_id": 1234567,
            "client_id": 70123456_i64,
            "name": name,
            "active": true,
            "color": "#06aaf5",
            "billable": null,
            "at": "2026-01-01T00:00:00+00:00"
        })
    }

    // --- pure mapping tests ----------------------------------------------

    #[test]
    fn maps_stopped_entry_with_project_name_tags_billable_and_local_times() {
        let names: HashMap<String, String> =
            [("9988776".to_string(), "Editorial".to_string())].into_iter().collect();
        let clients: HashMap<String, i64> = [("9988776".to_string(), 70123456_i64)].into_iter().collect();
        let e = entry_from(&entry_stopped(), &names, &clients).unwrap();
        assert_eq!(e.source, "toggl-track");
        assert_eq!(e.id, "3691827456", "id is the entry id (stable, the dedupe key)");
        assert_eq!(e.description, "Quarterly traffic report");
        assert_eq!(e.project, "Editorial", "project_id resolved to NAME");
        assert_eq!(e.duration_secs, Some(5400));
        assert_eq!(e.billable, Some(true));
        assert_eq!(e.tags, vec!["deep-work"]);
        assert!(e.client.is_empty(), "client NAME not resolved (no /me/clients call)");
        assert_eq!(
            e.extra.get("client_id"),
            Some(&json!(70123456_i64)),
            "client linkage preserved via the project's client_id, in extra",
        );
        // start = the UTC start, converted to local (same instant).
        assert_eq!(
            DateTime::parse_from_rfc3339(&e.start).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T16:00:00+00:00").unwrap().timestamp(),
        );
        // end = the UTC stop, converted to local (same instant).
        assert_eq!(
            DateTime::parse_from_rfc3339(&e.end).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T17:30:00+00:00").unwrap().timestamp(),
        );
        // extra carries the source-specific ids (numeric where the source is).
        assert_eq!(e.extra.get("workspace_id"), Some(&json!(1234567)));
        assert_eq!(e.extra.get("project_id"), Some(&json!(9988776)));
        assert_eq!(e.extra.get("user_id"), Some(&json!(42)));
        assert_eq!(e.extra.get("tag_ids"), Some(&json!([555])));
        assert!(e.extra.get("task_id").is_none(), "null task_id not stored");
    }

    #[test]
    fn running_timer_omits_end_and_duration() {
        let e = entry_from(&entry_running(), &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(e.id, "3691900001");
        assert!(e.end.is_empty(), "running timer has no end");
        assert!(e.duration_secs.is_none(), "running timer has no duration_secs");
        assert!(e.extra.get("client_id").is_none(), "no project→client mapping → no client_id");
        assert_eq!(e.billable, Some(false));
        // No project name resolved (empty map) → project blank, omitted.
        assert!(e.project.is_empty());
        let re = serde_json::to_value(&e).unwrap();
        assert!(re.get("end").is_none() && re.get("duration_secs").is_none());
        // billable false is still recorded (Some(false) → serialized).
        assert_eq!(re.get("billable"), Some(&json!(false)));
    }

    #[test]
    fn deleted_tombstone_is_detected() {
        assert!(is_deleted(&entry_deleted()), "non-null server_deleted_at ⇒ deleted");
        assert!(!is_deleted(&entry_stopped()), "null server_deleted_at ⇒ live");
        assert!(!is_deleted(&entry_running()), "absent server_deleted_at ⇒ live");
    }

    #[test]
    fn project_id_name_needs_both() {
        assert_eq!(
            project_id_name(&project_json(9988776, "Editorial")),
            Some(("9988776".into(), "Editorial".into()))
        );
        assert!(project_id_name(&json!({"id": 1})).is_none(), "no name → none");
        assert!(project_id_name(&json!({"name": "x"})).is_none(), "no id → none");
    }

    #[test]
    fn project_id_client_only_when_client_present() {
        // A project with a client_id yields the linkage.
        assert_eq!(
            project_id_client(&project_json(9988776, "Editorial")),
            Some(("9988776".into(), 70123456_i64))
        );
        // A client-less project (null client_id, the v9 shape for "no client")
        // yields nothing — we only record the linkage where it exists.
        assert!(
            project_id_client(&json!({"id": 9988776, "name": "Internal", "client_id": null})).is_none(),
            "null client_id → no linkage"
        );
        assert!(
            project_id_client(&json!({"id": 9988776, "name": "Internal"})).is_none(),
            "absent client_id → no linkage"
        );
        assert!(project_id_client(&json!({"client_id": 5})).is_none(), "no id → none");
    }

    #[test]
    fn max_at_epoch_picks_the_latest() {
        let entries = vec![entry_stopped(), entry_deleted(), entry_running()];
        // The running entry's at (2026-06-15T15:00:01Z) is the latest across
        // the whole drain — including past the tombstone, so a re-poll won't
        // re-fetch anything in the window.
        let w = max_at_epoch(&entries).unwrap();
        assert_eq!(w, to_epoch("2026-06-15T15:00:01+00:00").unwrap());
    }

    // --- a scripted mock API ---------------------------------------------

    struct MockApi {
        projects: Vec<Value>,
        entries: RefCell<Vec<Result<Vec<Value>, FetchError>>>,
        since_seen: RefCell<Vec<Option<i64>>>,
    }

    impl MockApi {
        fn new(projects: Vec<Value>, entries: Vec<Value>) -> Self {
            MockApi {
                projects,
                entries: RefCell::new(vec![Ok(entries)]),
                since_seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl TogglApi for MockApi {
        fn projects(&self) -> Result<Vec<Value>, FetchError> {
            Ok(self.projects.clone())
        }
        fn time_entries(&self, since: Option<i64>) -> Result<Vec<Value>, FetchError> {
            self.since_seen.borrow_mut().push(since);
            self.entries
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_skips_tombstone_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(
            vec![project_json(9988776, "Editorial")],
            vec![entry_stopped(), entry_running(), entry_deleted()],
        );

        let out = pull_with(&v, &api).unwrap();
        // Two live entries written (stopped + running); the tombstone skipped.
        assert_eq!(out.counts.get("entries"), Some(&2), "tombstone excluded from contract");

        // Contract stream, partitioned by local month of `start`.
        let jun = std::fs::read_to_string(v.root().join("time-entries/toggl-track/2026-06.jsonl")).unwrap();
        assert_eq!(jun.lines().count(), 2, "stopped + running, not the tombstone");
        assert!(jun.contains("\"id\":\"3691827456\""), "stopped entry present");
        assert!(jun.contains("\"id\":\"3691900001\""), "running entry present");
        assert!(!jun.contains("3690000000"), "tombstone id never in the contract stream");
        assert!(jun.contains("\"project\":\"Editorial\""), "project resolved to name");
        assert!(jun.contains("\"duration_secs\":5400"));

        // Raw layer mirrors the partitioning, verbatim API objects (keeps the
        // fields the contract drops, e.g. the raw project_id and at).
        let raw = std::fs::read_to_string(v.root().join("time-entries/toggl-track/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("\"workspace_id\":1234567"));
        assert!(raw.contains("\"duration\":-1"), "raw keeps the running entry's negative duration");

        // Watermark advanced to the max `at` across the drain (the running
        // entry's, the latest — past the tombstone, so it's not re-fetched).
        let state = v.read_toggl_sync();
        assert_eq!(state.since, to_epoch("2026-06-15T15:00:01+00:00"));
        assert!(state.updated.is_some());
        // The cursor file carries NO token.
        let cursor = std::fs::read_to_string(v.root().join(".trove/toggl-track-sync.json")).unwrap();
        assert!(!cursor.contains("api_token") && !cursor.contains("access_token"));

        // First call sent no `since`; the watermark is now set for next time.
        assert_eq!(api.since_seen.borrow().as_slice(), &[None]);

        // Re-run with the same input → id dedupe, byte-identical contract file.
        let api2 = MockApi::new(
            vec![project_json(9988776, "Editorial")],
            vec![entry_stopped(), entry_running(), entry_deleted()],
        );
        let again = pull_with(&v, &api2).unwrap();
        assert_eq!(again.counts.get("entries"), Some(&0), "no new ids on re-sync");
        let jun2 = std::fs::read_to_string(v.root().join("time-entries/toggl-track/2026-06.jsonl")).unwrap();
        assert_eq!(jun, jun2, "contract file byte-identical after re-run");
        // The re-sync sent the stored watermark as `since`.
        assert_eq!(api2.since_seen.borrow().as_slice(), &[to_epoch("2026-06-15T15:00:01+00:00")]);
    }

    #[test]
    fn running_then_stopped_keeps_open_contract_row_but_raw_gets_both_snapshots() {
        let v = temp_vault("running-stop");
        // Sync 1: the entry is running (no end/duration).
        let api1 = MockApi::new(vec![project_json(9988776, "Editorial")], vec![entry_running()]);
        pull_with(&v, &api1).unwrap();
        let after1 = std::fs::read_to_string(v.root().join("time-entries/toggl-track/2026-06.jsonl")).unwrap();
        assert_eq!(after1.lines().count(), 1);
        assert!(!after1.contains("duration_secs"), "still running");

        // Sync 2: the SAME id now stopped (end + positive duration). The
        // contract stream is append-only (ContractKind::EventStream) and dedupes
        // by id, so the stopped row is NOT re-appended — the *first observed*
        // (open) row stays in the normalized stream. This is the documented
        // append-only behavior (time_entries.rs / domains/time-entries.md): the
        // closed end/duration are NOT lost — they land in the per-source raw/
        // snapshot, which keeps every fetched object verbatim. (There is no
        // last-wins time-entries reader, by design: an open row contributes no
        // closed duration; a reader wanting the final figure reads raw/.)
        let stopped_same_id = json!({
            "id": 3691900001_i64,
            "workspace_id": 1234567,
            "project_id": 9988776,
            "user_id": 42,
            "billable": false,
            "start": "2026-06-15T15:00:00+00:00",
            "stop": "2026-06-15T16:00:00+00:00",
            "duration": 3600,
            "description": "Writing the collector",
            "tags": [],
            "at": "2026-06-15T16:00:00+00:00",
            "server_deleted_at": null
        });
        let api2 = MockApi::new(vec![project_json(9988776, "Editorial")], vec![stopped_same_id]);
        let out = pull_with(&v, &api2).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&0), "same id deduped, not re-appended to the contract");
        let after2 = std::fs::read_to_string(v.root().join("time-entries/toggl-track/2026-06.jsonl")).unwrap();
        assert_eq!(after2.lines().count(), 1, "contract still one (open) line for the id");
        assert!(!after2.contains("duration_secs"), "contract row stays the first-observed open state");

        // The closed end/duration are preserved — in raw/, where the second
        // fetch's verbatim object was appended alongside the first.
        let raw = std::fs::read_to_string(v.root().join("time-entries/toggl-track/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 2, "raw keeps BOTH snapshots (running + stopped)");
        assert!(raw.contains("\"duration\":-1"), "raw has the running snapshot");
        assert!(raw.contains("\"duration\":3600"), "raw has the stopped snapshot — final duration not lost");
    }

    #[test]
    fn fetch_error_does_not_advance_watermark() {
        let v = temp_vault("fetcherr");
        // Seed a watermark so we can prove it is untouched on a failed drain.
        v.write_toggl_sync(&SyncState { since: Some(1000), updated: None }).unwrap();
        let api = MockApi {
            projects: vec![project_json(9988776, "Editorial")],
            entries: RefCell::new(vec![Err(FetchError::Other("boom".into()))]),
            since_seen: RefCell::new(Vec::new()),
        };
        let err = pull_with(&v, &api).unwrap_err().to_string();
        assert!(err.contains("time_entries"), "error names the endpoint: {err}");
        // Watermark unchanged: the next sync re-asks from the same `since`.
        assert_eq!(v.read_toggl_sync().since, Some(1000), "failed drain must not advance");
    }

    #[test]
    fn auth_header_is_basic_token_colon_api_token() {
        let c = TogglClient::new(API_BASE.into(), "MYTOKEN".into());
        let h = c.auth_header();
        assert!(h.starts_with("Basic "));
        let b64 = h.strip_prefix("Basic ").unwrap();
        let decoded = String::from_utf8(STANDARD.decode(b64).unwrap()).unwrap();
        assert_eq!(decoded, "MYTOKEN:api_token", "Toggl Basic scheme: token as user, api_token as pass");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        // An empty cursor file deserializes to all-None (a first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.since.is_none());
        assert!(empty.updated.is_none());
        // An older cursor that carried only `since` still deserializes
        // (additive evolution — prove old lines load).
        let partial: SyncState = serde_json::from_str(r#"{"since":1717000000}"#).unwrap();
        assert_eq!(partial.since, Some(1717000000));
        assert!(partial.updated.is_none());
    }

    // --- connection tests -------------------------------------------------

    #[test]
    fn connection_stores_token_0600_and_absent_from_cursor() {
        let v = temp_vault("conn");
        // Store directly (def_connect needs the network for /me).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "tgl_secret_abc".into(),
                refresh_token: None,
                token_type: Some("Basic".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Toggl Track");
        assert_eq!(status.accounts[0].key, "toggl-track");

        // The token is NOT in any non-secret file (the cursor).
        v.write_toggl_sync(&SyncState { since: Some(1717000000), updated: Some("2026-06-15T00:00:00-07:00".into()) }).unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/toggl-track-sync.json")).unwrap();
        assert!(!cursor.contains("tgl_secret_abc"), "token never in the cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("tgl_secret_abc") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret token file must be 0600");
                }
            }
            assert!(found, "the token was stored under .trove/sync");
        }

        def_disconnect(&v, "toggl-track").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
    }

    #[test]
    fn empty_token_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "toggl-track");
        assert_eq!(DEF.connection, Some("toggl-track"));
    }
}
