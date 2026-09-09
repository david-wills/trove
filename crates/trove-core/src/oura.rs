//! Oura collector — every v2 API collection, full-history backfill, keyed
//! upserts. The OAuth/PAT side (connect flow, token store, refresh) is owned
//! by [`crate::sync::oura`]; this module only asks it for a fresh token.
//!
//! Layout under `health/oura/`:
//!
//! - `<collection>.jsonl` — raw Oura records, one per line, sorted by key.
//!   Rewritten whole on change (a year of daily records is ~365 lines).
//! - `heartrate/YYYY-MM.jsonl` — continuous heart rate is ~300 records/day,
//!   so it is month-partitioned and only touched months are rewritten.
//! - `personal_info.json` / `ring_configuration.json` — small singletons,
//!   overwritten each sync (`ring_configuration` only exists on PAT
//!   connections: no OAuth scope covers that endpoint and Oura answers
//!   401 for OAuth tokens, so the OAuth path skips it).
//! - `index.md` — human-readable summary table, regenerated each sync.
//!
//! Unlike TickTick, Oura's API serves arbitrary historical date ranges, so
//! everything is backfillable: the first sync seeds the last 30 days (data
//! on screen fast), then walks windows backward until the account start
//! (detected by consecutive empty windows), resumable at any interruption
//! via the per-collection cursor in `.trove/oura-sync.json`.
//!
//! Records are *upserted by key*, not appended: Oura recalculates recent
//! days after late ring syncs, so every incremental pass re-pulls a few
//! days behind the watermark and replaces what changed. Daily collections
//! key by `day` (robust even when a recalculated record returns under a new
//! `id`); event collections key by `id`; heartrate keys by `timestamp`.
//!
//! Sync runs inside the watcher owner loop (see [`crate::runner`]) with a
//! per-pass request budget so a long backfill can't stall activity
//! sampling; the manual pull (Integrations tab) is unbudgeted and drains
//! the whole backfill in one go.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::vault::Vault;

/// Seconds between Oura syncs in the watcher loop. Hourly: data only lands
/// a few times a day, after the ring syncs through the phone app.
pub const OURA_SYNC_SECS: u64 = 3600;

// Request-budgeted so a history backfill can't monopolize the owner loop; a
// silent no-op when no token is provisioned.
fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_oura(Some(OURA_LOOP_BUDGET))?;
    Ok(crate::registry::CollectOutcome::note_if(s.records > 0, || {
        format!(
            "oura synced — {} records across {} collections",
            s.records, s.collections
        )
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_oura_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

// Manual "Sync now": the unbudgeted pull, its typed stats mapped into the
// generic outcome. Errors (not connected, rate limited) pass through.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.oura_pull()?;
    let headline = if s.records == 0 {
        "Oura is up to date — no new records".to_string()
    } else if s.backfill_done {
        format!("Oura synced — {} records across {} collections", s.records, s.collections)
    } else {
        // Unbudgeted pulls normally drain the backfill; soft per-collection
        // errors can leave it unfinished, so say so rather than imply done.
        format!(
            "Oura synced — {} records across {} collections (history backfill still in progress)",
            s.records, s.collections
        )
    };
    Ok(PullOutcome {
        headline,
        counts: std::collections::BTreeMap::from([
            ("records", s.records),
            ("collections", u64::from(s.collections)),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "oura",
        name: "Oura Ring",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Pulls every Oura collection — sleep, readiness, activity, stress, resilience, SpO2, heart rate, workouts, tags — hourly, with a full-history backfill on first connect.",
        domain: "health",
        vault_path: "health/oura/",
        toggleable: true,
        setup: &[
            "Easiest: paste a Personal Access Token from cloud.ouraring.com (account → Personal Access Tokens).",
            "Or bring your own OAuth app: register one at cloud.ouraring.com/oauth/applications with redirect URI http://localhost:38574/callback, then paste its Client ID and Secret here once — every later connect is just a login.",
            "Connect from this card; the first sync backfills your entire ring history.",
        ],
        caveats: "Data exists only after the ring syncs through the phone app, so expect hours of lag. Oura recalculates recent days — the last week is re-pulled and upserted every pass. Personal Access Tokens are deprecated by Oura (existing ones work) and never refresh; OAuth tokens last ~30 days and refresh automatically. ring_configuration.json is PAT-only: no OAuth scope covers that endpoint, so OAuth connections skip it.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(OURA_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("oura"),
    pull: Some(def_pull),
};

/// Request budget for one watcher-loop pass. Plenty for steady-state
/// incremental syncs (~20 requests); caps how long a backfill can occupy
/// the owner loop. Far under Oura's 5000 requests / 5 min limit.
pub const OURA_LOOP_BUDGET: u32 = 150;

const SYNC_FILE: &str = ".trove/oura-sync.json";
const OURA_API: &str = "https://api.ouraring.com";
/// Kept short for the same reason as the tasks sync: a hung connection
/// must not stall the watcher owner loop for long.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Days re-pulled behind the watermark each incremental pass — Oura
/// recalculates recent days, the keyed upsert absorbs the overlap.
const OVERLAP_DAYS_DAILY: i64 = 7;
const OVERLAP_DAYS_DATETIME: i64 = 1;
/// First-connect seed window (recent data lands before the backfill).
const SEED_DAYS: i64 = 30;
/// Backfill window sizes walking backward.
const BACKFILL_DAYS_DAILY: i64 = 30;
const BACKFILL_DAYS_DATETIME: i64 = 7;
/// Consecutive empty backfill windows that mean "reached the account
/// start". Generous enough to walk over a few months of ring hiatus.
const BACKFILL_EMPTY_STOP: u32 = 4;
/// Hard floor — Oura predates nothing before this; a belt-and-braces stop
/// for the backfill walk.
const OURA_EPOCH: &str = "2013-01-01";

/// How a collection is queried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// `start_date`/`end_date` (YYYY-MM-DD).
    Daily,
    /// `start_datetime`/`end_datetime` (heartrate only — high volume).
    Datetime,
}

/// Which record field is the upsert key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Day,
    Id,
    Timestamp,
}

/// One date-ranged API collection and how to store it.
struct Collection {
    /// File stem under `health/oura/` and the sync-state key.
    name: &'static str,
    /// Path segment in the API URL. Differs from `name` only for
    /// `vO2_max`, which the API spells with a capital O.
    api_path: &'static str,
    window: Window,
    key: Key,
    /// Month-partitioned files (heartrate volume).
    monthly: bool,
}

const fn daily(name: &'static str) -> Collection {
    Collection { name, api_path: name, window: Window::Daily, key: Key::Day, monthly: false }
}

const fn event(name: &'static str) -> Collection {
    Collection { name, api_path: name, window: Window::Daily, key: Key::Id, monthly: false }
}

/// Every date-ranged collection the v2 API serves (the deprecated `tag` is
/// skipped in favor of `enhanced_tag`).
static COLLECTIONS: &[Collection] = &[
    daily("daily_activity"),
    daily("daily_cardiovascular_age"),
    daily("daily_readiness"),
    daily("daily_resilience"),
    daily("daily_sleep"),
    daily("daily_spo2"),
    daily("daily_stress"),
    daily("sleep_time"),
    Collection {
        name: "vo2_max",
        api_path: "vO2_max",
        window: Window::Daily,
        key: Key::Day,
        monthly: false,
    },
    event("enhanced_tag"),
    event("rest_mode_period"),
    event("session"),
    event("sleep"),
    event("workout"),
    Collection {
        name: "heartrate",
        api_path: "heartrate",
        window: Window::Datetime,
        key: Key::Timestamp,
        monthly: true,
    },
];

/// Un-ranged endpoints, fetched whole every sync.
static SINGLETONS: &[&str] = &["personal_info", "ring_configuration"];

/// Result of one sync pass, for logging/the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct OuraSyncStats {
    /// Collections that gained or changed records this pass.
    pub collections: u32,
    /// Records written (new + updated) this pass.
    pub records: u64,
    /// Every collection has finished walking back to the account start.
    pub backfill_done: bool,
}

/// Per-collection sync progress, persisted in `.trove/oura-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct OuraCollectionState {
    /// Newest day (YYYY-MM-DD) synced through; incremental passes resume
    /// a few days behind it (recalculation overlap).
    pub watermark: Option<String>,
    /// Earliest window-start the backfill has walked back to; the next
    /// backfill window ends here. Deleting the sync file re-walks history.
    pub backfill_cursor: Option<String>,
    pub backfill_done: bool,
    /// Empty windows seen in a row — `BACKFILL_EMPTY_STOP` of them ends
    /// the walk.
    #[serde(default)]
    pub empty_windows: u32,
    /// Total records ever written for this collection (drives index.md).
    pub records: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct OuraSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Why the last attempt failed (expired token, rate limit), for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub collections: BTreeMap<String, OuraCollectionState>,
}

/// Status-level fetch errors: 401 and 429 need distinct handling (abort
/// the pass vs. retry next cadence), everything else is just a message.
enum FetchError {
    RateLimited,
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

struct Page {
    data: Vec<Value>,
    next_token: Option<String>,
}

/// Thin v2 API client. The base URL is injected so the sync logic below it
/// stays testable against a local stub (the `tasks.rs` pattern).
struct OuraClient {
    base: String,
    token: String,
}

impl OuraClient {
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

    fn fetch_page(
        &self,
        c: &Collection,
        start: &str,
        end: &str,
        next_token: Option<&str>,
    ) -> Result<Page, FetchError> {
        let mut params: Vec<(&str, String)> = match c.window {
            Window::Daily => vec![
                ("start_date", start.to_string()),
                ("end_date", end.to_string()),
            ],
            Window::Datetime => {
                let off = Local::now().format("%:z").to_string();
                vec![
                    ("start_datetime", format!("{start}T00:00:00{off}")),
                    ("end_datetime", format!("{end}T23:59:59{off}")),
                ]
            }
        };
        if let Some(t) = next_token {
            params.push(("next_token", t.to_string()));
        }
        let v = self.get(&format!("/v2/usercollection/{}", c.api_path), &params)?;
        Ok(Page {
            data: v
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            next_token: v
                .get("next_token")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    fn fetch_singleton(&self, name: &str) -> Result<Value, FetchError> {
        self.get(&format!("/v2/usercollection/{name}"), &[])
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

/// What one window pull produced.
enum Outcome {
    Fetched { fetched: u64, new: u64, updated: u64 },
    OutOfBudget,
}

fn parse_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// The upsert key of one record, per the collection's key kind.
fn record_key(key: Key, record: &Value) -> Option<String> {
    let field = match key {
        Key::Day => "day",
        Key::Id => "id",
        Key::Timestamp => "timestamp",
    };
    record.get(field).and_then(Value::as_str).map(str::to_string)
}

/// Merge fetched records into the previous set by key — replace on
/// collision, keep order sorted by key. Returns (merged, new, updated);
/// re-fetching identical records counts as neither.
fn merge_records(prev: Vec<Value>, fetched: Vec<Value>, key: Key) -> (Vec<Value>, u64, u64) {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    for r in prev {
        if let Some(k) = record_key(key, &r) {
            map.insert(k, r);
        }
    }
    let (mut new, mut updated) = (0, 0);
    for r in fetched {
        let Some(k) = record_key(key, &r) else { continue };
        match map.get(&k) {
            None => {
                new += 1;
                map.insert(k, r);
            }
            Some(old) if *old != r => {
                updated += 1;
                map.insert(k, r);
            }
            Some(_) => {}
        }
    }
    (map.into_values().collect(), new, updated)
}

/// The next incremental window: a few days behind the watermark (Oura
/// recalculates recent days) through today; without a watermark, the
/// first-connect seed window.
fn incremental_range(watermark: Option<&str>, today: NaiveDate, window: Window) -> (String, String) {
    let overlap = match window {
        Window::Daily => OVERLAP_DAYS_DAILY,
        Window::Datetime => OVERLAP_DAYS_DATETIME,
    };
    let start = watermark
        .and_then(parse_day)
        .map(|w| w - chrono::Duration::days(overlap))
        .unwrap_or(today - chrono::Duration::days(SEED_DAYS))
        .min(today);
    (start.to_string(), today.to_string())
}

/// The next backfill window walking backward from the cursor. `None` on a
/// corrupt cursor (the caller treats that collection as done).
fn next_backfill_window(cursor: &str, window: Window) -> Option<(String, String)> {
    let days = match window {
        Window::Daily => BACKFILL_DAYS_DAILY,
        Window::Datetime => BACKFILL_DAYS_DATETIME,
    };
    let end = parse_day(cursor)?;
    let start = end - chrono::Duration::days(days);
    Some((start.to_string(), end.to_string()))
}

impl Vault {
    /// One Oura sync pass: refresh the token if needed, fetch the
    /// singletons, bring every collection up to today, then continue the
    /// historical backfill until done or the budget runs out. A silent
    /// no-op when no token is provisioned; failures are recorded in
    /// `.trove/oura-sync.json` and never corrupt progress — the next pass
    /// resumes from the persisted watermarks/cursors.
    pub fn collect_oura(&self, budget: Option<u32>) -> Result<OuraSyncStats> {
        self.collect_oura_against(OURA_API, budget)
    }

    /// [`Vault::collect_oura`] with an injectable API base. Production
    /// always calls it with [`OURA_API`]; tests point it at a local stub so
    /// the summary-freshness hook below can be exercised end to end without
    /// hitting the network.
    fn collect_oura_against(&self, base: &str, budget: Option<u32>) -> Result<OuraSyncStats> {
        if self.load_sync_token("oura")?.is_none() {
            return Ok(OuraSyncStats::default());
        }
        let mut state = self.read_oura_sync().unwrap_or_default();
        let token = match crate::sync::oura::fresh_token(self) {
            Ok(t) => t,
            Err(e) => {
                state.updated = Local::now().to_rfc3339();
                state.error = Some(format!("{e:#}"));
                self.write_oura_sync(&state)?;
                return Err(e);
            }
        };
        let client = OuraClient {
            base: base.to_string(),
            token: token.access_token,
        };
        let result = self.oura_sync_pass(&client, &mut state, Budget(budget));
        state.updated = Local::now().to_rfc3339();
        state.error = result.as_ref().err().map(|e| format!("{e:#}"));
        self.write_oura_sync(&state)?;
        self.write_oura_index(&state)?;
        // Freshness hook: both the watcher loop and manual "Sync now" funnel
        // through here, so the unified-health summary (health_unified.rs)
        // is always caught up right after a sync writes or removes data —
        // reads never have to reparse raw JSONL themselves.
        self.ensure_oura_summary()?;
        result
    }

    fn oura_sync_pass(
        &self,
        client: &OuraClient,
        state: &mut OuraSyncState,
        mut budget: Budget,
    ) -> Result<OuraSyncStats> {
        let today = Local::now().date_naive();
        let mut stats = OuraSyncStats::default();
        let mut touched: BTreeSet<&str> = BTreeSet::new();
        let mut soft_errors: Vec<String> = Vec::new();
        // Collections the token can't read (a scope unchecked at consent).
        // personal_info passing above means the token itself is fine, so a
        // collection-level 401 is a scope gap: skip the collection for the
        // rest of the pass, sync everything else, surface it once.
        let mut unauthorized: BTreeSet<&str> = BTreeSet::new();
        let done = |state: &OuraSyncState| {
            COLLECTIONS
                .iter()
                .all(|c| state.collections.get(c.name).is_some_and(|s| s.backfill_done))
        };

        // Singletons first: cheap, and a bad token aborts the pass before
        // any range budget is spent. Only personal_info's 401 is
        // token-fatal — it is covered by the always-requested `personal`
        // scope, so a 401 there means the token itself is bad. No OAuth
        // scope covers ring_configuration at all: Oura serves it to PATs
        // but answers 401 for OAuth tokens even when every scope was
        // granted, so that 401 says nothing about the token. Skip it; PAT
        // connections still get the file.
        for name in SINGLETONS {
            if !budget.take() {
                return Ok(stats);
            }
            match client.fetch_singleton(name) {
                Ok(v) => self.write_oura_singleton(name, &v)?,
                Err(FetchError::Unauthorized) if *name != "personal_info" => {}
                Err(e) => return Err(status_error(name, e)),
            }
        }

        // Incremental: bring every collection up to today.
        for c in COLLECTIONS {
            let watermark = state
                .collections
                .get(c.name)
                .and_then(|s| s.watermark.clone());
            let (start, end) = incremental_range(watermark.as_deref(), today, c.window);
            match self.pull_window(client, c, &start, &end, &mut budget) {
                Ok(Outcome::Fetched { new, updated, .. }) => {
                    let cstate = state.collections.entry(c.name.to_string()).or_default();
                    if cstate.backfill_cursor.is_none() && !cstate.backfill_done {
                        // The backfill picks up where the seed window began.
                        cstate.backfill_cursor = Some(start.clone());
                    }
                    cstate.watermark = Some(end.clone());
                    cstate.records += new;
                    stats.records += new + updated;
                    if new + updated > 0 {
                        touched.insert(c.name);
                    }
                }
                Ok(Outcome::OutOfBudget) => {
                    stats.collections = touched.len() as u32;
                    return Ok(stats);
                }
                Err(FetchError::Unauthorized) => {
                    unauthorized.insert(c.name);
                    soft_errors.push(unauthorized_note(c.name));
                }
                Err(FetchError::Other(msg)) => soft_errors.push(format!("{}: {msg}", c.name)),
                Err(e) => return Err(status_error(c.name, e)),
            }
        }
        self.write_oura_sync(state)?;

        // Backfill: walk history backward, round-robin so no collection
        // starves, persisting the cursor after every window so an
        // interrupted walk resumes exactly where it stopped.
        loop {
            let mut progressed = false;
            for c in COLLECTIONS {
                if unauthorized.contains(c.name) {
                    continue;
                }
                let cstate = state.collections.entry(c.name.to_string()).or_default();
                if cstate.backfill_done {
                    continue;
                }
                let Some(cursor) = cstate.backfill_cursor.clone() else {
                    continue; // incremental for this collection failed this pass
                };
                let window = next_backfill_window(&cursor, c.window)
                    .filter(|_| cursor.as_str() > OURA_EPOCH);
                let Some((start, end)) = window else {
                    cstate.backfill_done = true;
                    continue;
                };
                match self.pull_window(client, c, &start, &end, &mut budget) {
                    Ok(Outcome::Fetched { fetched, new, updated }) => {
                        let cstate = state.collections.entry(c.name.to_string()).or_default();
                        if fetched == 0 {
                            cstate.empty_windows += 1;
                            if cstate.empty_windows >= BACKFILL_EMPTY_STOP {
                                cstate.backfill_done = true;
                            }
                        } else {
                            cstate.empty_windows = 0;
                        }
                        cstate.backfill_cursor = Some(start);
                        cstate.records += new;
                        stats.records += new + updated;
                        if new + updated > 0 {
                            touched.insert(c.name);
                        }
                        progressed = true;
                        self.write_oura_sync(state)?;
                    }
                    Ok(Outcome::OutOfBudget) => {
                        stats.collections = touched.len() as u32;
                        return Ok(stats);
                    }
                    Err(FetchError::Unauthorized) => {
                        unauthorized.insert(c.name);
                        soft_errors.push(unauthorized_note(c.name));
                    }
                    Err(FetchError::Other(msg)) => {
                        soft_errors.push(format!("{}: {msg}", c.name))
                    }
                    Err(e) => return Err(status_error(c.name, e)),
                }
            }
            if !progressed {
                break;
            }
        }
        stats.collections = touched.len() as u32;
        stats.backfill_done = done(state);
        if !soft_errors.is_empty() {
            // Progress for the failed collections wasn't advanced — they
            // retry next pass. Surface the failure for the log/UI.
            soft_errors.truncate(3);
            bail!("oura sync hit errors: {}", soft_errors.join("; "));
        }
        Ok(stats)
    }

    /// Fetch one date window, following pagination, and upsert the records.
    fn pull_window(
        &self,
        client: &OuraClient,
        c: &Collection,
        start: &str,
        end: &str,
        budget: &mut Budget,
    ) -> Result<Outcome, FetchError> {
        let mut records = Vec::new();
        let mut next: Option<String> = None;
        loop {
            if !budget.take() {
                // Drop the partial window — watermark/cursor not advanced,
                // so the next pass re-pulls it whole.
                return Ok(Outcome::OutOfBudget);
            }
            let page = client.fetch_page(c, start, end, next.as_deref())?;
            records.extend(page.data);
            next = page.next_token;
            if next.is_none() {
                break;
            }
        }
        let fetched = records.len() as u64;
        let (new, updated) = self
            .apply_oura_records(c, records)
            .map_err(|e| FetchError::Other(format!("{e:#}")))?;
        Ok(Outcome::Fetched { fetched, new, updated })
    }

    /// Upsert fetched records into the collection's file(s).
    fn apply_oura_records(&self, c: &Collection, fetched: Vec<Value>) -> Result<(u64, u64)> {
        if fetched.is_empty() {
            return Ok((0, 0));
        }
        let (mut new, mut updated) = (0, 0);
        if c.monthly {
            // Partition by the record's month and rewrite only touched
            // month files (heartrate: ~300 records/day).
            let mut by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
            for r in fetched {
                let Some(k) = record_key(c.key, &r) else { continue };
                if k.len() < 7 {
                    continue;
                }
                by_month.entry(k[..7].to_string()).or_default().push(r);
            }
            for (month, recs) in by_month {
                let rel = format!("health/oura/{}/{month}.jsonl", c.name);
                let prev = self.load_oura_records(&rel)?;
                let (merged, n, u) = merge_records(prev, recs, c.key);
                self.write_oura_records(&rel, &merged)?;
                new += n;
                updated += u;
            }
        } else {
            let rel = format!("health/oura/{}.jsonl", c.name);
            let prev = self.load_oura_records(&rel)?;
            let (merged, n, u) = merge_records(prev, fetched, c.key);
            self.write_oura_records(&rel, &merged)?;
            new = n;
            updated = u;
        }
        Ok((new, updated))
    }

    pub(crate) fn load_oura_records(&self, rel: &str) -> Result<Vec<Value>> {
        let path = self.resolve(rel)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    fn write_oura_records(&self, rel: &str, records: &[Value]) -> Result<()> {
        self.write_snapshot(rel, records)
    }

    /// Small un-ranged payloads, pretty-printed and overwritten each sync.
    fn write_oura_singleton(&self, name: &str, v: &Value) -> Result<()> {
        // Collection-shaped singletons (ring_configuration) store just the
        // data array; personal_info is already a bare object.
        let v = v.get("data").cloned().unwrap_or_else(|| v.clone());
        crate::store::write_json_atomic(&self.resolve(&format!("health/oura/{name}.json"))?, &v)
    }

    /// The persisted sync progress, if a sync has ever run.
    pub fn read_oura_sync(&self) -> Option<OuraSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Atomic write so readers never see a torn file. Called after every
    /// backfill window — this file is what makes the walk resumable.
    fn write_oura_sync(&self, state: &OuraSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Regenerate the human-readable summary.
    fn write_oura_index(&self, state: &OuraSyncState) -> Result<()> {
        let mut md = format!(
            "# Oura Ring\n\nLast sync: {}\n\n| Collection | Records | Synced through | History |\n|---|---|---|---|\n",
            state.updated
        );
        for c in COLLECTIONS {
            let s = state.collections.get(c.name).cloned().unwrap_or_default();
            let history = if s.backfill_done {
                "complete".to_string()
            } else {
                s.backfill_cursor
                    .as_deref()
                    .map(|cur| format!("backfilled to {cur}"))
                    .unwrap_or_else(|| "—".into())
            };
            md.push_str(&format!(
                "| {} | {} | {} | {history} |\n",
                c.name,
                s.records,
                s.watermark.as_deref().unwrap_or("—"),
            ));
        }
        crate::store::write_atomic(&self.resolve("health/oura/index.md")?, md.as_bytes())
    }
}

/// Soft-error note for a collection the token can't read. The advice is
/// reconnect-and-keep-everything-checked because Oura's consent page lets
/// the user untick individual scopes.
fn unauthorized_note(collection: &str) -> String {
    format!(
        "{collection}: unauthorized (401) — the connected account didn't grant its scope; reconnect and keep every permission checked"
    )
}

fn status_error(collection: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::RateLimited => {
            anyhow!("Oura rate limited the sync ({collection}) — it will resume next pass")
        }
        FetchError::Unauthorized => {
            anyhow!("Oura rejected the token (401) — reconnect from the Integrations tab")
        }
        FetchError::Other(m) => anyhow!("oura {collection}: {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-oura-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn col(name: &str) -> &'static Collection {
        COLLECTIONS.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn merge_upserts_by_day_even_when_id_changes() {
        // Oura recalculates daily records and may reissue them under a new
        // id — keying by day must replace, never duplicate.
        let prev = vec![json!({"id": "a1", "day": "2026-06-01", "score": 70})];
        let fetched = vec![json!({"id": "b2", "day": "2026-06-01", "score": 75})];
        let (merged, new, updated) = merge_records(prev, fetched, Key::Day);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["score"], 75);
        assert_eq!((new, updated), (0, 1));
    }

    #[test]
    fn merge_counts_new_unchanged_and_updated_by_id() {
        let prev = vec![
            json!({"id": "a", "v": 1}),
            json!({"id": "b", "v": 2}),
        ];
        let fetched = vec![
            json!({"id": "a", "v": 1}),  // identical — neither new nor updated
            json!({"id": "b", "v": 9}),  // changed
            json!({"id": "c", "v": 3}),  // new
            json!({"v": 4}),             // keyless — skipped
        ];
        let (merged, new, updated) = merge_records(prev, fetched, Key::Id);
        assert_eq!(merged.len(), 3);
        assert_eq!((new, updated), (1, 1));
        // Sorted by key.
        let keys: Vec<&str> = merged.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(keys, ["a", "b", "c"]);
    }

    #[test]
    fn incremental_range_overlaps_behind_the_watermark() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        let (start, end) = incremental_range(Some("2026-06-10"), today, Window::Daily);
        assert_eq!((start.as_str(), end.as_str()), ("2026-06-03", "2026-06-11"));
        let (start, _) = incremental_range(Some("2026-06-10"), today, Window::Datetime);
        assert_eq!(start, "2026-06-09");
    }

    #[test]
    fn incremental_range_seeds_without_a_watermark() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        let (start, end) = incremental_range(None, today, Window::Daily);
        assert_eq!((start.as_str(), end.as_str()), ("2026-05-12", "2026-06-11"));
        // A garbled watermark falls back to the seed window too.
        let (start, _) = incremental_range(Some("not-a-date"), today, Window::Daily);
        assert_eq!(start, "2026-05-12");
        // A future watermark (clock skew) never produces start > end.
        let (start, end) = incremental_range(Some("2026-07-01"), today, Window::Daily);
        assert!(start <= end);
    }

    #[test]
    fn backfill_window_walks_backward() {
        let (start, end) = next_backfill_window("2026-05-12", Window::Daily).unwrap();
        assert_eq!((start.as_str(), end.as_str()), ("2026-04-12", "2026-05-12"));
        let (start, end) = next_backfill_window("2026-05-12", Window::Datetime).unwrap();
        assert_eq!((start.as_str(), end.as_str()), ("2026-05-05", "2026-05-12"));
        assert!(next_backfill_window("garbage", Window::Daily).is_none());
    }

    #[test]
    fn heartrate_partitions_by_month() {
        let v = temp_vault("hr");
        let fetched = vec![
            json!({"bpm": 60, "source": "ppg", "timestamp": "2026-05-31T23:55:00+00:00"}),
            json!({"bpm": 62, "source": "ppg", "timestamp": "2026-06-01T00:05:00+00:00"}),
            json!({"bpm": 64, "source": "ppg", "timestamp": "2026-06-01T00:10:00+00:00"}),
        ];
        let (new, updated) = v.apply_oura_records(col("heartrate"), fetched).unwrap();
        assert_eq!((new, updated), (3, 0));
        assert!(v.root().join("health/oura/heartrate/2026-05.jsonl").exists());
        assert!(v.root().join("health/oura/heartrate/2026-06.jsonl").exists());
        let june = v.load_oura_records("health/oura/heartrate/2026-06.jsonl").unwrap();
        assert_eq!(june.len(), 2);

        // Re-applying the same records is a no-op (idempotent overlap).
        let again = vec![json!({"bpm": 62, "source": "ppg", "timestamp": "2026-06-01T00:05:00+00:00"})];
        let (new, updated) = v.apply_oura_records(col("heartrate"), again).unwrap();
        assert_eq!((new, updated), (0, 0));
    }

    #[test]
    fn records_round_trip_through_the_vault() {
        let v = temp_vault("roundtrip");
        let fetched = vec![
            json!({"id": "w2", "day": "2026-06-02", "activity": "run"}),
            json!({"id": "w1", "day": "2026-06-01", "activity": "walk"}),
        ];
        v.apply_oura_records(col("workout"), fetched).unwrap();
        let stored = v.load_oura_records("health/oura/workout.jsonl").unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0]["id"], "w1", "sorted by key");
    }

    /// Byte-parity contract for the four write paths (records rewrite,
    /// singleton, sync state, index.md): exact bytes, pinned before the port
    /// onto `store` and unchanged by it.
    #[test]
    fn writes_are_byte_identical() {
        let v = temp_vault("parity");
        v.apply_oura_records(
            col("workout"),
            vec![
                json!({"id": "w2", "day": "2026-06-02", "activity": "run"}),
                json!({"id": "w1", "day": "2026-06-01", "activity": "walk"}),
            ],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join("health/oura/workout.jsonl")).unwrap(),
            "{\"activity\":\"walk\",\"day\":\"2026-06-01\",\"id\":\"w1\"}\n\
             {\"activity\":\"run\",\"day\":\"2026-06-02\",\"id\":\"w2\"}\n",
            "merged records rewrite, sorted by key"
        );

        v.write_oura_singleton("personal_info", &json!({"age": 38, "email": "user@example.com"}))
            .unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join("health/oura/personal_info.json")).unwrap(),
            "{\n  \"age\": 38,\n  \"email\": \"user@example.com\"\n}"
        );

        let mut state = OuraSyncState::default();
        state.updated = "2026-06-11T10:00:00-07:00".into();
        state.collections.insert(
            "daily_sleep".into(),
            OuraCollectionState {
                watermark: Some("2026-06-11".into()),
                backfill_cursor: Some("2024-01-01".into()),
                backfill_done: false,
                empty_windows: 2,
                records: 700,
            },
        );
        v.write_oura_sync(&state).unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join(".trove/oura-sync.json")).unwrap(),
            "{\n  \"updated\": \"2026-06-11T10:00:00-07:00\",\n  \"collections\": {\n    \"daily_sleep\": {\n      \"watermark\": \"2026-06-11\",\n      \"backfill_cursor\": \"2024-01-01\",\n      \"backfill_done\": false,\n      \"empty_windows\": 2,\n      \"records\": 700\n    }\n  }\n}"
        );

        v.write_oura_index(&state).unwrap();
        let mut expected = String::from(
            "# Oura Ring\n\nLast sync: 2026-06-11T10:00:00-07:00\n\n\
             | Collection | Records | Synced through | History |\n|---|---|---|---|\n",
        );
        for name in [
            "daily_activity",
            "daily_cardiovascular_age",
            "daily_readiness",
            "daily_resilience",
            "daily_sleep",
            "daily_spo2",
            "daily_stress",
            "sleep_time",
            "vo2_max",
            "enhanced_tag",
            "rest_mode_period",
            "session",
            "sleep",
            "workout",
            "heartrate",
        ] {
            if name == "daily_sleep" {
                expected.push_str("| daily_sleep | 700 | 2026-06-11 | backfilled to 2024-01-01 |\n");
            } else {
                expected.push_str(&format!("| {name} | 0 | — | — |\n"));
            }
        }
        assert_eq!(
            fs::read_to_string(v.root().join("health/oura/index.md")).unwrap(),
            expected
        );
    }

    #[test]
    fn sync_state_round_trips_and_records_errors() {
        let v = temp_vault("state");
        assert!(v.read_oura_sync().is_none());
        let mut state = OuraSyncState {
            updated: "2026-06-11T10:00:00-07:00".into(),
            error: Some("rate limited".into()),
            ..Default::default()
        };
        state.collections.insert(
            "daily_sleep".into(),
            OuraCollectionState {
                watermark: Some("2026-06-11".into()),
                backfill_cursor: Some("2024-01-01".into()),
                backfill_done: false,
                empty_windows: 2,
                records: 700,
            },
        );
        v.write_oura_sync(&state).unwrap();
        let loaded = v.read_oura_sync().unwrap();
        assert_eq!(loaded.error.as_deref(), Some("rate limited"));
        let sleep = &loaded.collections["daily_sleep"];
        assert_eq!(sleep.records, 700);
        assert_eq!(sleep.empty_windows, 2);
        assert_eq!(sleep.backfill_cursor.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn index_lists_every_collection() {
        let v = temp_vault("index");
        let mut state = OuraSyncState::default();
        state.updated = "2026-06-11T10:00:00-07:00".into();
        state.collections.insert(
            "daily_sleep".into(),
            OuraCollectionState {
                watermark: Some("2026-06-11".into()),
                backfill_done: true,
                records: 700,
                ..Default::default()
            },
        );
        v.write_oura_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join("health/oura/index.md")).unwrap();
        for c in COLLECTIONS {
            assert!(md.contains(c.name), "index missing {}", c.name);
        }
        assert!(md.contains("| daily_sleep | 700 | 2026-06-11 | complete |"));
    }

    #[test]
    fn collect_without_token_is_a_silent_noop() {
        let v = temp_vault("notoken");
        let stats = v.collect_oura(Some(10)).unwrap();
        assert_eq!(stats.records, 0);
        assert!(!v.root().join("health/oura").exists());
    }

    #[test]
    fn manual_pull_without_token_is_an_error() {
        // Unlike the watcher's silent no-op above, the user-triggered pull
        // must say why nothing happened.
        let v = temp_vault("pullnotoken");
        let err = def_pull(&v).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn pat_tokens_never_expire() {
        // The PAT path stores expires_at: None — `expired()` must stay
        // false so the refresh machinery never fires for PATs.
        let pat = crate::sync::oauth::TokenSet {
            access_token: "pat".into(),
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at: None,
        };
        assert!(!pat.expired());
    }

    // -- full passes against a local stub: 401 handling per endpoint ------

    /// Minimal HTTP/1.1 stub mapping the request target to a canned
    /// (status, JSON body). The listener thread outlives the test
    /// harmlessly (the youtube.rs pattern, plus status codes).
    fn stub_server(
        route: impl Fn(&str) -> (u16, String) + Send + 'static,
    ) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
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
                let (status, body) = route(&target);
                let reason = if status == 200 { "OK" } else { "Error" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        base
    }

    fn empty_page() -> (u16, String) {
        (200, json!({"data": [], "next_token": null}).to_string())
    }

    #[test]
    fn ring_configuration_401_does_not_abort_the_pass() {
        // No OAuth scope covers ring_configuration, so Oura 401s it for
        // every OAuth token — the pass must skip it and still sync the
        // collections (this exact 401 used to brick the whole sync).
        let v = temp_vault("ringcfg401");
        let base = stub_server(|target| {
            if target.starts_with("/v2/usercollection/personal_info") {
                return (200, json!({"age": 38}).to_string());
            }
            if target.starts_with("/v2/usercollection/ring_configuration") {
                return (401, json!({"detail": "no scope"}).to_string());
            }
            if target.starts_with("/v2/usercollection/daily_sleep") {
                return (
                    200,
                    json!({"data": [{"id": "s1", "day": "2026-06-11", "score": 80}],
                           "next_token": null})
                    .to_string(),
                );
            }
            empty_page()
        });
        let client = OuraClient { base, token: "tok".into() };
        let mut state = OuraSyncState::default();
        let stats = v
            .oura_sync_pass(&client, &mut state, Budget(None))
            .expect("ring_configuration 401 must not fail the pass");
        assert!(stats.records >= 1);
        assert!(v.root().join("health/oura/personal_info.json").exists());
        assert!(!v.root().join("health/oura/ring_configuration.json").exists());
        assert!(v.root().join("health/oura/daily_sleep.jsonl").exists());
        assert!(state.collections["daily_sleep"].watermark.is_some());
    }

    #[test]
    fn personal_info_401_still_means_bad_token() {
        let v = temp_vault("badtoken");
        let base = stub_server(|target| {
            if target.starts_with("/v2/usercollection/personal_info") {
                return (401, json!({"detail": "invalid token"}).to_string());
            }
            empty_page()
        });
        let client = OuraClient { base, token: "bad".into() };
        let mut state = OuraSyncState::default();
        let err = v
            .oura_sync_pass(&client, &mut state, Budget(None))
            .unwrap_err();
        assert!(err.to_string().contains("reconnect"), "{err}");
        assert!(!v.root().join("health/oura").exists());
    }

    #[test]
    fn collection_401_is_soft_and_the_rest_still_sync() {
        // A scope the user unticked at consent: that collection reports an
        // error, every other collection syncs and advances normally.
        let v = temp_vault("scopegap");
        let base = stub_server(|target| {
            if target.starts_with("/v2/usercollection/personal_info") {
                return (200, json!({"age": 38}).to_string());
            }
            if target.starts_with("/v2/usercollection/workout") {
                return (401, json!({"detail": "no scope"}).to_string());
            }
            if target.starts_with("/v2/usercollection/daily_sleep") {
                return (
                    200,
                    json!({"data": [{"id": "s1", "day": "2026-06-11", "score": 80}],
                           "next_token": null})
                    .to_string(),
                );
            }
            empty_page()
        });
        let client = OuraClient { base, token: "tok".into() };
        let mut state = OuraSyncState::default();
        let err = v
            .oura_sync_pass(&client, &mut state, Budget(None))
            .unwrap_err();
        assert!(err.to_string().contains("workout"), "{err}");
        assert!(err.to_string().contains("permission"), "{err}");
        // The other collections were not blocked by workout's 401.
        assert!(v.root().join("health/oura/daily_sleep.jsonl").exists());
        assert!(state.collections["daily_sleep"].watermark.is_some());
        // workout's progress was not advanced — it retries next pass.
        assert!(state.collections.get("workout").is_none_or(|s| s.watermark.is_none()));
    }

    /// health-refactor Step 2: `collect_oura` (the real entrypoint both the
    /// watcher loop and manual "Sync now" call) must leave
    /// `.trove/oura-summary.json` fresh and consistent with what was just
    /// synced, not just the raw JSONL.
    #[test]
    fn collect_oura_refreshes_the_summary_index() {
        let v = temp_vault("summary-hook");
        let base = stub_server(|target| {
            if target.starts_with("/v2/usercollection/personal_info") {
                return (200, json!({"age": 38}).to_string());
            }
            if target.starts_with("/v2/usercollection/daily_sleep") {
                return (
                    200,
                    json!({"data": [{"id": "s1", "day": "2026-06-11", "score": 80}],
                           "next_token": null})
                    .to_string(),
                );
            }
            empty_page()
        });
        v.save_sync_token(
            "oura",
            &crate::sync::oauth::TokenSet {
                access_token: "tok".into(),
                refresh_token: None,
                token_type: Some("Bearer".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        // A small budget: enough to cover the incremental pass (singletons
        // + every collection) so daily_sleep's data lands, without letting
        // the backfill (which never sees an empty window for daily_sleep,
        // since the stub always answers it) run away.
        let stats = v.collect_oura_against(&base, Some(20)).unwrap();
        assert!(stats.records >= 1, "{stats:?}");

        let summary_path = v.root().join(".trove/oura-summary.json");
        assert!(summary_path.exists(), "collect_oura must refresh the summary");
        let summary: Value =
            serde_json::from_str(&fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert_eq!(summary["version"], 1);
        assert!(
            summary["files"].get("daily_sleep.jsonl").is_some(),
            "{summary}"
        );
        assert_eq!(summary["metrics"]["sleep-score"][0]["day"], "2026-06-11");
        assert_eq!(summary["metrics"]["sleep-score"][0]["sum"], 80.0);
        assert_eq!(summary["metrics"]["sleep-score"][0]["count"], 1);
    }

    #[test]
    fn catalog_paths_and_keys_are_consistent() {
        let mut seen = BTreeSet::new();
        for c in COLLECTIONS {
            assert!(seen.insert(c.name), "duplicate collection {}", c.name);
            assert!(
                c.name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit()),
                "bad file stem: {}",
                c.name
            );
            if c.monthly {
                assert_eq!(c.key, Key::Timestamp, "monthly partitioning keys by timestamp");
            }
        }
    }
}
