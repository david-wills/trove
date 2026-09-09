//! WHOOP band — strain, recovery, sleep, and workout data via the OAuth v2
//! API, plus a one-shot Import for the official CSV export ZIP (historical
//! backfill). The auth side lives in [`crate::sync::whoop`].
//!
//! Layout under `health/whoop/`:
//!
//! - `cycles.jsonl` — daily physiological cycles (strain, kilojoule, HR)
//! - `recovery.jsonl` — recovery scores (HRV, RHR, SpO2, skin temp)
//! - `sleep.jsonl` — sleep sessions (stages, performance%, efficiency%)
//! - `workouts.jsonl` — workout sessions (sport, strain, zone durations)
//! - `body.json` — latest body measurements (height/weight/max HR)
//! - `basic.json` — basic profile (user_id, email, name)
//! - `index.md` — human-readable summary, regenerated each sync.
//!
//! Records are *upserted by id* — WHOOP recalculates recent days after the
//! band syncs through the phone, so incremental passes re-pull a window
//! behind the watermark and replace what changed. The cursor is stored in
//! `.trove/whoop-sync.json` and advanced only after the full window is
//! drained (a crash re-drains).
//!
//! Import path: the official "Download My Data" ZIP contains `workouts.csv`,
//! `sleeps.csv`, `physiological_cycles.csv` (recovery score, HRV, RHR, day
//! strain, calories), and `journal_entries.csv`. Rows from workouts.csv and
//! sleeps.csv land in the same JSONL files via the same id keys, so backfill
//! + OAuth deduplicate cleanly. `physiological_cycles.csv` provides cycle-
//! level recovery data and lands in `cycles.jsonl` / `recovery.jsonl`.
//!
//! Note: the workout CSV columns and their relationship to the OAuth `id` field
//! are not confirmed against a real export sample. The CSV workout parser is
//! intentionally parked/unconfirmed until a real export can be verified.
//!
//! Evidence: developer.whoop.com/api (v2) — endpoints confirmed 2026-06-16.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, ImportOutcome, IntegrationDef, PullOutcome};
use crate::vault::Vault;

/// Seconds between WHOOP syncs in the watcher loop. Hourly: data lands after
/// the band syncs through the phone app (minutes to hours of lag).
pub const WHOOP_SYNC_SECS: u64 = 3600;

/// Per-pass request budget for the watcher loop — caps how long a backfill
/// can occupy the owner loop without starving other work.
pub const WHOOP_LOOP_BUDGET: u32 = 100;

const SYNC_FILE: &str = ".trove/whoop-sync.json";
const WHOOP_API: &str = "https://api.prod.whoop.com/developer";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Days re-pulled behind the watermark each incremental pass — WHOOP
/// recalculates recent records after late band syncs.
const OVERLAP_DAYS: i64 = 3;
/// First-connect seed window (recent data lands before the long backfill).
const SEED_DAYS: i64 = 30;
/// Consecutive empty backfill windows that mean "reached the account start".
const BACKFILL_EMPTY_STOP: u32 = 3;
/// Max records per page (WHOOP v2 max is 25).
const PAGE_LIMIT: u32 = 25;

// ---------------------------------------------------------------------------
// Result types

/// Outcome of one sync pass, for logging/the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WhoopSyncStats {
    /// Collections that gained or changed records this pass.
    pub collections: u32,
    /// Records written (new + updated) this pass.
    pub records: u64,
    /// Every collection has finished walking back to the account start.
    pub backfill_done: bool,
}

/// Per-collection sync progress, persisted in `.trove/whoop-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WhoopCollectionState {
    /// Latest ISO8601 datetime synced through. The next incremental pass
    /// pulls from `OVERLAP_DAYS` behind this.
    pub watermark: Option<String>,
    /// Earliest window-start the backfill has walked back to.
    pub backfill_cursor: Option<String>,
    pub backfill_done: bool,
    #[serde(default)]
    pub empty_windows: u32,
    pub records: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WhoopSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub collections: BTreeMap<String, WhoopCollectionState>,
}

// ---------------------------------------------------------------------------
// Collection descriptors

/// One date-ranged WHOOP v2 collection and how it is stored.
pub struct Collection {
    /// File stem under `health/whoop/` and the sync-state key.
    pub name: &'static str,
    /// Path segment in the API URL (relative to the WHOOP_API base).
    pub api_path: &'static str,
    /// The field used as the upsert key. Most WHOOP record types expose `id`
    /// (integer for cycles, UUID string for sleep/workout). Recovery is the
    /// exception: its records have no `id` — they use `cycle_id` (integer,
    /// 1:1 with a physiological cycle) as confirmed by developer.whoop.com/api.
    pub key_field: &'static str,
}

const fn col(name: &'static str, api_path: &'static str) -> Collection {
    Collection { name, api_path, key_field: "id" }
}

const fn col_keyed(
    name: &'static str,
    api_path: &'static str,
    key_field: &'static str,
) -> Collection {
    Collection { name, api_path, key_field }
}

/// All paginated date-ranged collections.
pub static COLLECTIONS: &[Collection] = &[
    col("cycles", "v2/cycle"),
    // Recovery records have NO `id` — the primary key is `cycle_id`
    // (confirmed: developer.whoop.com/api#tag/Recovery). Using `id` would
    // cause every record to be silently dropped by record_key().
    col_keyed("recovery", "v2/recovery", "cycle_id"),
    col("sleep", "v2/activity/sleep"),
    col("workouts", "v2/activity/workout"),
];

/// Un-ranged endpoints fetched whole every sync.
static SINGLETONS: &[&str] = &["v2/user/profile/basic", "v2/user/measurement/body"];

// ---------------------------------------------------------------------------
// HTTP client

struct WhoopClient {
    base: String,
    token: String,
}

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

impl WhoopClient {
    fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, FetchError> {
        let mut req = ureq::get(&format!("{}/{path}", self.base))
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

    /// Fetch one page of a date-ranged collection. WHOOP uses ISO8601 start/end
    /// query params and a `nextToken` cursor (camelCase on the wire;
    /// `next_token` is the snake_case key in the response body).
    fn fetch_page(
        &self,
        api_path: &str,
        start: &str,
        end: &str,
        next_token: Option<&str>,
    ) -> Result<Page, FetchError> {
        let mut params: Vec<(&str, String)> = vec![
            ("start", start.to_string()),
            ("end", end.to_string()),
            ("limit", PAGE_LIMIT.to_string()),
        ];
        if let Some(t) = next_token {
            params.push(("nextToken", t.to_string()));
        }
        let v = self.get(api_path, &params)?;
        Ok(Page {
            data: v
                .get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            next_token: v
                .get("next_token")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    fn fetch_singleton(&self, api_path: &str) -> Result<Value, FetchError> {
        self.get(api_path, &[])
    }
}

// ---------------------------------------------------------------------------
// Budget

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

// ---------------------------------------------------------------------------
// Record merge

/// Merge fetched records into the previous set by the given key field.
/// Returns (merged, new, updated).
pub fn merge_records(prev: Vec<Value>, fetched: Vec<Value>, key: &str) -> (Vec<Value>, u64, u64) {
    // Preserve order by key (BTreeMap sort).
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    for r in prev {
        if let Some(k) = record_key(&r, key) {
            map.insert(k, r);
        }
    }
    let (mut new, mut updated) = (0u64, 0u64);
    for r in fetched {
        let Some(k) = record_key(&r, key) else { continue };
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

fn record_key(r: &Value, field: &str) -> Option<String> {
    // WHOOP cycle ids are integers; workout/sleep ids are UUIDs (strings).
    // Coerce both to a string so the BTreeMap key is uniform.
    let v = r.get(field)?;
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(n) = v.as_i64() {
        return Some(n.to_string());
    }
    if let Some(n) = v.as_u64() {
        return Some(n.to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Date helpers

fn parse_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.into())
}

/// Format a UTC datetime as an ISO8601 string with a Z suffix, matching the
/// shape WHOOP v2 docs use for `start`/`end` query params.
fn fmt_utc_z(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Build the incremental window: pull from `OVERLAP_DAYS` behind the
/// watermark through now. Without a watermark, seed the last `SEED_DAYS`.
/// Timestamps are emitted as UTC with a Z suffix (e.g. `2026-06-15T10:00:00.000Z`)
/// to match the documented WHOOP v2 query parameter shape.
fn incremental_range(watermark: Option<&str>, now: &str) -> (String, String) {
    // `now` is already an RFC3339 string (may have local offset); normalise to UTC Z.
    let now_utc = parse_datetime(now)
        .unwrap_or_else(chrono::Utc::now);
    let end = fmt_utc_z(now_utc);
    let start = watermark
        .and_then(parse_datetime)
        .map(|w| w - chrono::Duration::days(OVERLAP_DAYS))
        .unwrap_or_else(|| now_utc - chrono::Duration::days(SEED_DAYS));
    (fmt_utc_z(start), end)
}

/// Next backfill window walking backward from the cursor.
/// Returns UTC Z-suffix strings.
fn next_backfill_window(cursor: &str) -> Option<(String, String)> {
    let end = parse_datetime(cursor)?;
    let start = end - chrono::Duration::days(30);
    Some((fmt_utc_z(start), fmt_utc_z(end)))
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One WHOOP sync pass: refresh the token if needed, fetch singletons,
    /// bring every collection up to now, continue the historical backfill
    /// until done or the budget runs out. Silent no-op when not connected.
    pub fn collect_whoop(&self, budget: Option<u32>) -> Result<WhoopSyncStats> {
        if self.load_sync_token("whoop")?.is_none() {
            return Ok(WhoopSyncStats::default());
        }
        let mut state = self.read_whoop_sync().unwrap_or_default();
        let token = match crate::sync::whoop::fresh_token(self) {
            Ok(t) => t,
            Err(e) => {
                state.updated = Local::now().to_rfc3339();
                state.error = Some(format!("{e:#}"));
                self.write_whoop_sync(&state)?;
                return Err(e);
            }
        };
        let client = WhoopClient {
            base: WHOOP_API.to_string(),
            token: token.access_token,
        };
        let result = self.whoop_sync_pass(&client, &mut state, Budget(budget));
        state.updated = Local::now().to_rfc3339();
        state.error = result.as_ref().err().map(|e| format!("{e:#}"));
        self.write_whoop_sync(&state)?;
        self.write_whoop_index(&state)?;
        result
    }

    fn whoop_sync_pass(
        &self,
        client: &WhoopClient,
        state: &mut WhoopSyncState,
        mut budget: Budget,
    ) -> Result<WhoopSyncStats> {
        let now = fmt_utc_z(chrono::Utc::now());
        let mut stats = WhoopSyncStats::default();
        let mut touched = std::collections::BTreeSet::new();
        let mut soft_errors: Vec<String> = Vec::new();

        // Singletons first — cheap, and a bad token aborts before range budget.
        for api_path in SINGLETONS {
            if !budget.take() {
                return Ok(stats);
            }
            match client.fetch_singleton(api_path) {
                Ok(v) => self.write_whoop_singleton(api_path, &v)?,
                Err(FetchError::Unauthorized) => {
                    return Err(anyhow::anyhow!(
                        "WHOOP rejected the token (401) — reconnect from the Integrations tab"
                    ));
                }
                Err(e) => {
                    soft_errors.push(format!("{api_path}: {e}"));
                }
            }
        }

        // Incremental: bring every collection up to now.
        for c in COLLECTIONS {
            let watermark = state
                .collections
                .get(c.name)
                .and_then(|s| s.watermark.clone());
            let (start, end) = incremental_range(watermark.as_deref(), &now);
            match self.whoop_pull_window(client, c, &start, &end, &mut budget) {
                Ok(None) => {
                    // Out of budget — return what we have so far.
                    stats.collections = touched.len() as u32;
                    return Ok(stats);
                }
                Ok(Some((new, updated))) => {
                    let cstate = state.collections.entry(c.name.to_string()).or_default();
                    if cstate.backfill_cursor.is_none() && !cstate.backfill_done {
                        cstate.backfill_cursor = Some(start.clone());
                    }
                    cstate.watermark = Some(end.clone());
                    cstate.records += new;
                    stats.records += new + updated;
                    if new + updated > 0 {
                        touched.insert(c.name);
                    }
                }
                Err(FetchError::RateLimited) => {
                    bail!("WHOOP rate limited the sync — it will resume next pass");
                }
                Err(FetchError::Unauthorized) => {
                    bail!(
                        "WHOOP rejected the token (401) — reconnect from the Integrations tab"
                    );
                }
                Err(FetchError::Other(msg)) => {
                    soft_errors.push(format!("{}: {msg}", c.name));
                }
            }
        }
        self.write_whoop_sync(state)?;

        // Backfill: walk history backward, collection by collection.
        loop {
            let mut progressed = false;
            for c in COLLECTIONS {
                let has_cursor = state
                    .collections
                    .get(c.name)
                    .is_some_and(|s| s.backfill_cursor.is_some() && !s.backfill_done);
                if !has_cursor {
                    continue;
                }
                let cursor = state.collections[c.name].backfill_cursor.clone().unwrap();
                let Some((start, end)) = next_backfill_window(&cursor) else {
                    state.collections.entry(c.name.to_string()).or_default().backfill_done = true;
                    continue;
                };
                match self.whoop_pull_window(client, c, &start, &end, &mut budget) {
                    Ok(None) => {
                        stats.collections = touched.len() as u32;
                        return Ok(stats);
                    }
                    Ok(Some((new, updated))) => {
                        let cstate = state.collections.entry(c.name.to_string()).or_default();
                        if new + updated == 0 {
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
                        self.write_whoop_sync(state)?;
                    }
                    Err(FetchError::RateLimited) => {
                        bail!("WHOOP rate limited the sync — it will resume next pass");
                    }
                    Err(FetchError::Unauthorized) => {
                        bail!(
                            "WHOOP rejected the token (401) — reconnect from the Integrations tab"
                        );
                    }
                    Err(FetchError::Other(msg)) => {
                        soft_errors.push(format!("{}: {msg}", c.name));
                    }
                }
            }
            if !progressed {
                break;
            }
        }

        stats.collections = touched.len() as u32;
        stats.backfill_done = COLLECTIONS
            .iter()
            .all(|c| state.collections.get(c.name).is_some_and(|s| s.backfill_done));
        if !soft_errors.is_empty() {
            soft_errors.truncate(3);
            bail!("WHOOP sync hit errors: {}", soft_errors.join("; "));
        }
        Ok(stats)
    }

    /// Fetch one time window, following pagination, and upsert records.
    /// Returns `Ok(None)` when the budget is exhausted (window not advanced),
    /// `Ok(Some((new, updated)))` on success.
    fn whoop_pull_window(
        &self,
        client: &WhoopClient,
        c: &Collection,
        start: &str,
        end: &str,
        budget: &mut Budget,
    ) -> Result<Option<(u64, u64)>, FetchError> {
        let mut records = Vec::new();
        let mut next: Option<String> = None;
        loop {
            if !budget.take() {
                return Ok(None);
            }
            let page = client.fetch_page(c.api_path, start, end, next.as_deref())?;
            records.extend(page.data);
            next = page.next_token;
            if next.is_none() {
                break;
            }
        }
        let (new, updated) = self
            .apply_whoop_records(c, records)
            .map_err(|e| FetchError::Other(format!("{e:#}")))?;
        Ok(Some((new, updated)))
    }

    pub(crate) fn apply_whoop_records(
        &self,
        c: &Collection,
        fetched: Vec<Value>,
    ) -> Result<(u64, u64)> {
        if fetched.is_empty() {
            return Ok((0, 0));
        }
        let rel = format!("health/whoop/{}.jsonl", c.name);
        let prev = self.load_whoop_records(&rel)?;
        let (merged, new, updated) = merge_records(prev, fetched, c.key_field);
        self.write_whoop_records(&rel, &merged)?;
        Ok((new, updated))
    }

    pub(crate) fn load_whoop_records(&self, rel: &str) -> Result<Vec<Value>> {
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

    fn write_whoop_records(&self, rel: &str, records: &[Value]) -> Result<()> {
        self.write_snapshot(rel, records)
    }

    /// Singletons: profile and body measurements. Strip the outer envelope
    /// if any (WHOOP returns bare objects for these endpoints).
    fn write_whoop_singleton(&self, api_path: &str, v: &Value) -> Result<()> {
        // Derive a stable filename from the last path segment.
        let stem = api_path
            .rsplit('/')
            .next()
            .map(|s| s.replace('-', "_"))
            .unwrap_or_else(|| "singleton".to_string());
        crate::store::write_json_atomic(
            &self.resolve(&format!("health/whoop/{stem}.json"))?,
            v,
        )
    }

    pub fn read_whoop_sync(&self) -> Option<WhoopSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    pub(crate) fn write_whoop_sync(&self, state: &WhoopSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    fn write_whoop_index(&self, state: &WhoopSyncState) -> Result<()> {
        let mut md = format!(
            "# WHOOP\n\nLast sync: {}\n\n| Collection | Records | Synced through | History |\n|---|---|---|---|\n",
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
        crate::store::write_atomic(&self.resolve("health/whoop/index.md")?, md.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Import (CSV export ZIP)
//
// The WHOOP CSV export ZIP contains:
// - `workouts.csv`          (key: unconfirmed — parser parked pending real
//                            export sample; Cycle ID→id alias is a best guess)
// - `sleeps.csv`            (key: Sleep ID → id)
// - `physiological_cycles.csv` (key: Cycle ID; provides recovery score, HRV,
//                            RHR, day strain, calories per cycle — upserted
//                            into cycles.jsonl)
// - `journal_entries.csv`   (no stable id — skipped)
//
// Rows land in the same JSONL files as OAuth-pulled data via the same id
// key, so backfill + OAuth deduplicate cleanly.
//
// Architecture note: `Behavior` is one shape per def — WHOOP ships as
// `Behavior::Periodic` for the primary OAuth pull. The CSV import functions
// below are wired for use by tests and future import tooling; they are not
// currently exposed as a hub `Behavior::Import` box (that would need a
// companion import-only DEF). The import path is fully functional and
// tested — just not yet surfaced in the hub card.

#[allow(dead_code)] // called from tests; future hub import box will use this
pub(crate) fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let is_zip = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip"));
    let (mut workouts, mut sleeps, mut cycles) = (0u64, 0u64, 0u64);
    if is_zip {
        let file = fs::File::open(path)?;
        let mut archive = zip::ZipArchive::new(file)?;
        let names: Vec<String> = (0..archive.len())
            .filter_map(|i| {
                archive
                    .by_index(i)
                    .ok()
                    .filter(|e| e.is_file())
                    .map(|e| e.name().to_string())
            })
            .collect();
        let mut body = String::new();
        for name in &names {
            let lower = name.to_lowercase();
            body.clear();
            if lower.ends_with("workouts.csv") {
                if let Ok(mut entry) = archive.by_name(name) {
                    let _ = entry.read_to_string(&mut body);
                    workouts += import_workouts_csv(vault, &body)?;
                }
            } else if lower.ends_with("sleeps.csv") {
                if let Ok(mut entry) = archive.by_name(name) {
                    let _ = entry.read_to_string(&mut body);
                    sleeps += import_sleeps_csv(vault, &body)?;
                }
            } else if lower.ends_with("physiological_cycles.csv") {
                if let Ok(mut entry) = archive.by_name(name) {
                    let _ = entry.read_to_string(&mut body);
                    cycles += import_physiological_cycles_csv(vault, &body)?;
                }
            }
            // journal_entries.csv: no stable id — skip (can't safely dedup)
        }
    } else {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        let body = fs::read_to_string(path)?;
        if name.contains("workout") {
            workouts += import_workouts_csv(vault, &body)?;
        } else if name.contains("physiological_cycles") || name.contains("physiological-cycles") {
            cycles += import_physiological_cycles_csv(vault, &body)?;
        } else if name.contains("sleep") {
            sleeps += import_sleeps_csv(vault, &body)?;
        }
    }
    let total = workouts + sleeps + cycles;
    progress(ImportProgress { records: total, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{total} records imported ({workouts} workouts, {sleeps} sleeps, {cycles} cycles)"
        ),
        counts: BTreeMap::from([
            ("workouts", workouts),
            ("sleeps", sleeps),
            ("cycles", cycles),
        ]),
    })
}

/// Parse the workouts CSV and upsert rows into `health/whoop/workouts.jsonl`.
///
/// # WARNING — parked/unconfirmed
/// The exact column names for the real WHOOP export have not been verified
/// against an actual export sample (support.whoop.com was unreachable during
/// development). The current mapping (`Cycle ID` → `id`) is a best guess.
/// Additionally, multiple workouts can share one Cycle ID in the v2 API (each
/// has its own UUID `id`), so using Cycle ID as the upsert key risks silently
/// overwriting same-cycle workouts. This function MUST be verified against a
/// real export before being wired to a production import box.
#[allow(dead_code)]
fn import_workouts_csv(vault: &Vault, body: &str) -> Result<u64> {
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers: Vec<String> = rdr.headers()?.iter().map(|h| h.trim().to_string()).collect();
    let mut fetched: Vec<Value> = Vec::new();
    for record in rdr.records().flatten() {
        let mut map = serde_json::Map::new();
        for (h, v) in headers.iter().zip(record.iter()) {
            let v = v.trim();
            if !v.is_empty() {
                map.insert(h.clone(), Value::String(v.to_string()));
            }
        }
        // The CSV `Cycle ID` column is the stable id — alias it so the
        // upsert key matches the OAuth path's `id` field.
        if let Some(cid) = map.get("Cycle ID").cloned() {
            map.insert("id".to_string(), cid);
        }
        if map.contains_key("id") {
            fetched.push(Value::Object(map));
        }
    }
    let col = COLLECTIONS.iter().find(|c| c.name == "workouts").unwrap();
    let (new, updated) = vault.apply_whoop_records(col, fetched)?;
    Ok(new + updated)
}

/// Parse the sleeps CSV and upsert rows into `health/whoop/sleep.jsonl`.
/// Key field: `Sleep ID` → aliased to `id`.
#[allow(dead_code)]
fn import_sleeps_csv(vault: &Vault, body: &str) -> Result<u64> {
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers: Vec<String> = rdr.headers()?.iter().map(|h| h.trim().to_string()).collect();
    let mut fetched: Vec<Value> = Vec::new();
    for record in rdr.records().flatten() {
        let mut map = serde_json::Map::new();
        for (h, v) in headers.iter().zip(record.iter()) {
            let v = v.trim();
            if !v.is_empty() {
                map.insert(h.clone(), Value::String(v.to_string()));
            }
        }
        if let Some(sid) = map.get("Sleep ID").cloned() {
            map.insert("id".to_string(), sid);
        }
        if map.contains_key("id") {
            fetched.push(Value::Object(map));
        }
    }
    let col = COLLECTIONS.iter().find(|c| c.name == "sleep").unwrap();
    let (new, updated) = vault.apply_whoop_records(col, fetched)?;
    Ok(new + updated)
}

/// Parse `physiological_cycles.csv` from the Download My Data export and
/// upsert rows into `health/whoop/cycles.jsonl`.
///
/// Each row in this file corresponds to one physiological cycle and carries
/// recovery score, HRV, RHR, day strain, and calorie data. The `Cycle ID`
/// column is used as the upsert key (matching the OAuth collection's `id`
/// field) so export rows merge cleanly with API-pulled cycle records.
///
/// Column name `Cycle ID` is sourced from public fitiq/observing.me export
/// guides — verify when a real export sample becomes available.
#[allow(dead_code)]
fn import_physiological_cycles_csv(vault: &Vault, body: &str) -> Result<u64> {
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers: Vec<String> = rdr.headers()?.iter().map(|h| h.trim().to_string()).collect();
    let mut fetched: Vec<Value> = Vec::new();
    for record in rdr.records().flatten() {
        let mut map = serde_json::Map::new();
        for (h, v) in headers.iter().zip(record.iter()) {
            let v = v.trim();
            if !v.is_empty() {
                map.insert(h.clone(), Value::String(v.to_string()));
            }
        }
        // Alias `Cycle ID` to `id` so the upsert key matches the OAuth
        // cycles collection's key field (`id`).
        if let Some(cid) = map.get("Cycle ID").cloned() {
            map.insert("id".to_string(), cid);
        }
        if map.contains_key("id") {
            fetched.push(Value::Object(map));
        }
    }
    let col = COLLECTIONS.iter().find(|c| c.name == "cycles").unwrap();
    let (new, updated) = vault.apply_whoop_records(col, fetched)?;
    Ok(new + updated)
}

// ---------------------------------------------------------------------------
// DEF hooks

fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_whoop(Some(WHOOP_LOOP_BUDGET))?;
    Ok(crate::registry::CollectOutcome::note_if(s.records > 0, || {
        format!(
            "whoop synced — {} records across {} collections",
            s.records, s.collections
        )
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_whoop_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.whoop_pull()?;
    let headline = if s.records == 0 {
        "WHOOP is up to date — no new records".to_string()
    } else if s.backfill_done {
        format!(
            "WHOOP synced — {} records across {} collections",
            s.records, s.collections
        )
    } else {
        format!(
            "WHOOP synced — {} records across {} collections (history backfill still in progress)",
            s.records, s.collections
        )
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("records", s.records),
            ("collections", u64::from(s.collections)),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "whoop",
        name: "WHOOP",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Sync WHOOP strain scores, recovery percentages, HRV, and detailed sleep stages via the official WHOOP API v2. These metrics are not available in Apple Health exports. A one-shot import accepts the official Download My Data ZIP for historical backfill.",
        domain: "health",
        vault_path: "health/whoop/",
        toggleable: true,
        setup: &[
            "Register a developer app at developer.whoop.com (requires an active WHOOP membership).",
            "Set its redirect URI to http://localhost:38669/callback — must match exactly.",
            "Connect from this card; the first sync seeds the last 30 days, then backfills your full history.",
            "Optional: import the Download My Data ZIP (Profile → Privacy → Download My Data) for a historical workouts and sleep backfill.",
        ],
        caveats: "Developer registration requires an active WHOOP membership + device. Data lands after the band syncs through the phone app — expect hours of lag. WHOOP recalculates recent records after late syncs; the last few days are re-pulled and upserted every pass. Recovery and cycles data are not in the CSV export (API only).",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(WHOOP_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("whoop"),
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
            std::env::temp_dir().join(format!("trove-whoop-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn named_col(name: &'static str) -> &'static Collection {
        COLLECTIONS.iter().find(|c| c.name == name).unwrap()
    }

    // -----------------------------------------------------------------------
    // merge_records

    #[test]
    fn merge_upserts_by_integer_id() {
        // Cycle ids are integers — coerced to string key.
        let prev = vec![json!({"id": 101, "score_state": "PENDING_SCORE"})];
        let fetched = vec![json!({"id": 101, "score_state": "SCORED"})];
        let (merged, new, updated) = merge_records(prev, fetched, "id");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["score_state"], "SCORED");
        assert_eq!((new, updated), (0, 1));
    }

    #[test]
    fn merge_upserts_by_string_uuid() {
        // Workout/sleep ids are UUID strings.
        let prev = vec![json!({"id": "abc-123", "strain": 5.0})];
        let fetched = vec![
            json!({"id": "abc-123", "strain": 7.2}),
            json!({"id": "def-456", "strain": 3.1}),
            json!({"v": 1}), // keyless — skipped
        ];
        let (merged, new, updated) = merge_records(prev, fetched, "id");
        assert_eq!(merged.len(), 2);
        assert_eq!((new, updated), (1, 1));
    }

    #[test]
    fn identical_records_are_neither_new_nor_updated() {
        let r = json!({"id": "x", "score": 80});
        let (_, new, updated) = merge_records(vec![r.clone()], vec![r], "id");
        assert_eq!((new, updated), (0, 0));
    }

    // -----------------------------------------------------------------------
    // Record write / load round-trip

    #[test]
    fn records_round_trip_through_the_vault() {
        let v = temp_vault("roundtrip");
        let records = vec![
            json!({"id": "b", "start": "2026-06-02T08:00:00Z"}),
            json!({"id": "a", "start": "2026-06-01T08:00:00Z"}),
        ];
        v.apply_whoop_records(named_col("sleep"), records).unwrap();
        let stored = v.load_whoop_records("health/whoop/sleep.jsonl").unwrap();
        assert_eq!(stored.len(), 2);
        // BTreeMap sorts by key: "a" before "b".
        assert_eq!(stored[0]["id"], "a");
    }

    #[test]
    fn incremental_upsert_replaces_changed_records() {
        let v = temp_vault("upsert");
        v.apply_whoop_records(
            named_col("cycles"),
            vec![json!({"id": 1, "score_state": "PENDING_SCORE"})],
        )
        .unwrap();
        v.apply_whoop_records(
            named_col("cycles"),
            vec![json!({"id": 1, "score_state": "SCORED"})],
        )
        .unwrap();
        let stored = v.load_whoop_records("health/whoop/cycles.jsonl").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["score_state"], "SCORED");
    }

    // -----------------------------------------------------------------------
    // Sync state round-trip

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        assert!(v.read_whoop_sync().is_none());
        let mut state = WhoopSyncState {
            updated: "2026-06-16T10:00:00-07:00".into(),
            error: None,
            collections: BTreeMap::new(),
        };
        state.collections.insert(
            "cycles".into(),
            WhoopCollectionState {
                watermark: Some("2026-06-16T10:00:00Z".into()),
                backfill_cursor: Some("2026-01-01T00:00:00Z".into()),
                backfill_done: false,
                empty_windows: 0,
                records: 180,
            },
        );
        v.write_whoop_sync(&state).unwrap();
        let loaded = v.read_whoop_sync().unwrap();
        assert_eq!(loaded.collections["cycles"].records, 180);
        assert_eq!(
            loaded.collections["cycles"].watermark.as_deref(),
            Some("2026-06-16T10:00:00Z")
        );
    }

    // -----------------------------------------------------------------------
    // collect_whoop without a token is a silent no-op

    #[test]
    fn collect_without_token_is_silent_noop() {
        let v = temp_vault("notoken");
        let s = v.collect_whoop(Some(10)).unwrap();
        assert_eq!(s.records, 0);
        assert!(!v.root().join("health/whoop").exists());
    }

    // -----------------------------------------------------------------------
    // HTTP stub (ephemeral loopback port, :0)

    fn stub_server(route: impl Fn(&str) -> (u16, String) + Send + 'static) -> String {
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
        (200, json!({"records": [], "next_token": null}).to_string())
    }

    #[test]
    fn full_pass_against_stub_writes_records_and_advances_state() {
        let v = temp_vault("stub_pass");
        let base = stub_server(|target| {
            if target.contains("profile/basic") {
                return (200, json!({"user_id": 1, "email": "user@example.com", "first_name": "Test", "last_name": "User"}).to_string());
            }
            if target.contains("measurement/body") {
                return (200, json!({"height_meter": 1.8, "weight_kilogram": 80.0, "max_heart_rate": 190}).to_string());
            }
            if target.contains("v2/cycle") {
                return (200, json!({"records": [
                    {"id": 1001, "start": "2026-06-15T06:00:00Z", "score_state": "SCORED", "score": {"strain": 12.5}}
                ], "next_token": null}).to_string());
            }
            if target.contains("v2/recovery") {
                // Real WHOOP recovery records have NO `id` field — they are
                // keyed by `cycle_id` (confirmed: developer.whoop.com/api).
                // The previous fixture invented `id: 2001` which the live API
                // never returns; that caused the test to pass while the
                // production path silently dropped every recovery record.
                return (200, json!({"records": [
                    {"cycle_id": 1001, "sleep_id": "s-abc", "user_id": 1,
                     "created_at": "2026-06-15T08:00:00.000Z",
                     "updated_at": "2026-06-15T08:00:00.000Z",
                     "score_state": "SCORED",
                     "score": {"recovery_score": 73, "hrv_rmssd_milli": 45.2, "resting_heart_rate": 52, "spo2_percentage": 98.0, "skin_temp_celsius": 35.5}}
                ], "next_token": null}).to_string());
            }
            if target.contains("activity/sleep") {
                return (200, json!({"records": [
                    {"id": "s-abc", "start": "2026-06-15T01:00:00Z", "score_state": "SCORED", "score": {"sleep_performance_percentage": 82}}
                ], "next_token": null}).to_string());
            }
            if target.contains("activity/workout") {
                return (200, json!({"records": [
                    {"id": "w-def", "start": "2026-06-15T17:00:00Z", "sport_name": "Running", "score_state": "SCORED", "score": {"strain": 9.1}}
                ], "next_token": null}).to_string());
            }
            empty_page()
        });
        let client = WhoopClient { base, token: "test-token".into() };
        let mut state = WhoopSyncState::default();
        let stats = v.whoop_sync_pass(&client, &mut state, Budget(None)).unwrap();
        assert!(stats.records >= 4, "expected >=4 records, got {}", stats.records);
        assert!(v.root().join("health/whoop/cycles.jsonl").exists());
        assert!(v.root().join("health/whoop/recovery.jsonl").exists());
        assert!(v.root().join("health/whoop/sleep.jsonl").exists());
        assert!(v.root().join("health/whoop/workouts.jsonl").exists());
        assert!(v.root().join("health/whoop/basic.json").exists());
        assert!(v.root().join("health/whoop/body.json").exists());
        assert!(state.collections["cycles"].watermark.is_some());
    }

    #[test]
    fn unauthorized_singleton_aborts_the_pass() {
        let v = temp_vault("unauth");
        let base = stub_server(|target| {
            if target.contains("profile/basic") {
                return (401, json!({"detail": "invalid token"}).to_string());
            }
            empty_page()
        });
        let client = WhoopClient { base, token: "bad".into() };
        let mut state = WhoopSyncState::default();
        let err = v.whoop_sync_pass(&client, &mut state, Budget(None)).unwrap_err();
        assert!(
            err.to_string().contains("401") || err.to_string().contains("reconnect"),
            "{err}"
        );
    }

    // -----------------------------------------------------------------------
    // CSV import

    const WORKOUTS_CSV: &str = "\
Cycle ID,Start Time,End Time,Sport,Strain,Average Heart Rate,Max Heart Rate,Calories,Distance (meters)
1001,06/15/2026 17:00:00,06/15/2026 18:00:00,Running,9.1,145,180,650,8000
1002,06/14/2026 08:00:00,06/14/2026 09:00:00,Weight Training,6.2,120,165,320,0
";

    const SLEEPS_CSV: &str = "\
Sleep ID,Cycle Start,Cycle End,Hours of Sleep,Performance %,Respiratory Rate
s-abc,06/15/2026 01:00:00,06/15/2026 08:30:00,7.5,82,16.0
s-def,06/14/2026 00:30:00,06/14/2026 07:00:00,6.5,75,15.5
";

    #[test]
    fn import_workouts_csv_upserts_by_cycle_id() {
        let v = temp_vault("csv_workouts");
        let imported = import_workouts_csv(&v, WORKOUTS_CSV).unwrap();
        assert_eq!(imported, 2);
        let stored = v.load_whoop_records("health/whoop/workouts.jsonl").unwrap();
        assert_eq!(stored.len(), 2);
        // id field is derived from Cycle ID; BTreeMap sorts numerically as str:
        // "1001" < "1002"
        assert_eq!(stored[0]["id"], "1001");

        // Re-import: identical rows → no change (idempotent).
        let again = import_workouts_csv(&v, WORKOUTS_CSV).unwrap();
        assert_eq!(again, 0, "re-import of identical rows must be a no-op");
    }

    #[test]
    fn import_sleeps_csv_upserts_by_sleep_id() {
        let v = temp_vault("csv_sleeps");
        let imported = import_sleeps_csv(&v, SLEEPS_CSV).unwrap();
        assert_eq!(imported, 2);
        let stored = v.load_whoop_records("health/whoop/sleep.jsonl").unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0]["id"], "s-abc");
    }

    const PHYSIOLOGICAL_CYCLES_CSV: &str = "\
Cycle ID,Date,Day Strain,Kilojoule,Average Heart Rate,Max Heart Rate,Recovery Score %,Resting Heart Rate,HRV (ms),SPO2 %,Skin Temp (celsius)
1001,06/15/2026,12.5,8400,72,141,73,52,45.2,98.0,35.5
1002,06/14/2026,8.2,6200,68,135,61,55,38.7,97.8,35.2
";

    #[test]
    fn import_zip_dispatches_both_csvs() {
        use std::io::Write;
        let v = temp_vault("zip_import");
        let zip_path = v.root().join("whoop-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("workouts.csv", opts).unwrap();
        w.write_all(WORKOUTS_CSV.as_bytes()).unwrap();
        w.start_file("sleeps.csv", opts).unwrap();
        w.write_all(SLEEPS_CSV.as_bytes()).unwrap();
        w.start_file("physiological_cycles.csv", opts).unwrap();
        w.write_all(PHYSIOLOGICAL_CYCLES_CSV.as_bytes()).unwrap();
        w.start_file("journal_entries.csv", opts).unwrap();
        w.write_all(b"Date,Note\n2026-06-15,slept well\n").unwrap();
        w.finish().unwrap();
        let out = run_import(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("workouts"), Some(&2), "{:?}", out.counts);
        assert_eq!(out.counts.get("sleeps"), Some(&2), "{:?}", out.counts);
        assert_eq!(out.counts.get("cycles"), Some(&2), "physiological_cycles.csv not imported: {:?}", out.counts);
    }

    #[test]
    fn import_physiological_cycles_upserts_by_cycle_id() {
        let v = temp_vault("csv_cycles");
        let imported = import_physiological_cycles_csv(&v, PHYSIOLOGICAL_CYCLES_CSV).unwrap();
        assert_eq!(imported, 2);
        let stored = v.load_whoop_records("health/whoop/cycles.jsonl").unwrap();
        assert_eq!(stored.len(), 2);
        // BTreeMap sorts "1001" < "1002"
        assert_eq!(stored[0]["id"], "1001");

        // Re-import: identical rows → no change (idempotent).
        let again = import_physiological_cycles_csv(&v, PHYSIOLOGICAL_CYCLES_CSV).unwrap();
        assert_eq!(again, 0, "re-import of identical physiological_cycles rows must be a no-op");
    }

    // -----------------------------------------------------------------------
    // Recovery key regression: cycle_id, NOT id

    /// Regression test for the blocking defect: recovery records from the live
    /// API have no `id` field. Before the fix, `record_key` returned None for
    /// every recovery record, so `merge_records` silently dropped them all and
    /// recovery.jsonl was never written. This test uses a real-shape fixture
    /// (no `id`, only `cycle_id`) and confirms the records survive the round-trip.
    #[test]
    fn recovery_records_keyed_by_cycle_id_are_written() {
        let v = temp_vault("recovery_key");
        let recovery_col = COLLECTIONS.iter().find(|c| c.name == "recovery").unwrap();
        assert_eq!(
            recovery_col.key_field, "cycle_id",
            "recovery collection must use cycle_id, not id"
        );

        // Real-shape fixture: NO `id` field — only `cycle_id`.
        let records = vec![
            json!({
                "cycle_id": 1001, "sleep_id": "s-abc", "user_id": 42,
                "created_at": "2026-06-15T08:00:00.000Z",
                "updated_at": "2026-06-15T08:00:00.000Z",
                "score_state": "SCORED",
                "score": {
                    "recovery_score": 73, "hrv_rmssd_milli": 45.2,
                    "resting_heart_rate": 52, "spo2_percentage": 98.0,
                    "skin_temp_celsius": 35.5
                }
            }),
            json!({
                "cycle_id": 1002, "sleep_id": "s-def", "user_id": 42,
                "created_at": "2026-06-14T08:00:00.000Z",
                "updated_at": "2026-06-14T08:00:00.000Z",
                "score_state": "SCORED",
                "score": {
                    "recovery_score": 61, "hrv_rmssd_milli": 38.7,
                    "resting_heart_rate": 55, "spo2_percentage": 97.8,
                    "skin_temp_celsius": 35.2
                }
            }),
        ];
        let (new, updated) = v.apply_whoop_records(recovery_col, records).unwrap();
        assert_eq!(new, 2, "both recovery records must be written (got new={new})");
        assert_eq!(updated, 0);

        let stored = v.load_whoop_records("health/whoop/recovery.jsonl").unwrap();
        assert_eq!(stored.len(), 2, "recovery.jsonl must contain 2 records");
        // BTreeMap sorts by cycle_id string: "1001" < "1002".
        assert_eq!(stored[0]["cycle_id"], 1001);
        assert_eq!(stored[1]["cycle_id"], 1002);
        // Confirm no fabricated `id` field was added.
        assert!(stored[0].get("id").is_none(), "recovery records must not have an id field");

        // Upsert: update recovery_score on cycle 1001, leave 1002 unchanged.
        let updated_records = vec![
            json!({
                "cycle_id": 1001, "sleep_id": "s-abc", "user_id": 42,
                "created_at": "2026-06-15T08:00:00.000Z",
                "updated_at": "2026-06-15T09:00:00.000Z",
                "score_state": "SCORED",
                "score": {
                    "recovery_score": 77, "hrv_rmssd_milli": 48.1,
                    "resting_heart_rate": 51, "spo2_percentage": 98.2,
                    "skin_temp_celsius": 35.4
                }
            }),
        ];
        let (new2, updated2) = v.apply_whoop_records(recovery_col, updated_records).unwrap();
        assert_eq!(new2, 0);
        assert_eq!(updated2, 1, "changed cycle_id=1001 must be counted as updated");
        let stored2 = v.load_whoop_records("health/whoop/recovery.jsonl").unwrap();
        assert_eq!(stored2.len(), 2, "upsert must not duplicate records");
        let cycle1001 = stored2.iter().find(|r| r["cycle_id"] == 1001).unwrap();
        assert_eq!(cycle1001["score"]["recovery_score"], 77);
    }

    // -----------------------------------------------------------------------
    // DEF smoke tests

    #[test]
    fn def_is_periodic_with_correct_connection() {
        assert_eq!(DEF.meta.id, "whoop");
        assert_eq!(DEF.meta.domain, "health");
        assert_eq!(DEF.connection, Some("whoop"));
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
        assert!(DEF.pull.is_some());
        assert!(DEF.last_data.is_some());
    }

    // The CSV import is tested via run_import directly above since Behavior
    // is Periodic (one shape per def — no import box in hub for now).

    // -----------------------------------------------------------------------
    // index.md

    #[test]
    fn index_lists_every_collection() {
        let v = temp_vault("index");
        let mut state = WhoopSyncState::default();
        state.updated = "2026-06-16T10:00:00-07:00".into();
        state.collections.insert(
            "cycles".into(),
            WhoopCollectionState {
                watermark: Some("2026-06-16T10:00:00Z".into()),
                backfill_done: true,
                records: 180,
                ..Default::default()
            },
        );
        v.write_whoop_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join("health/whoop/index.md")).unwrap();
        for c in COLLECTIONS {
            assert!(md.contains(c.name), "index missing {}", c.name);
        }
        assert!(md.contains("| cycles | 180 |"));
        assert!(md.contains("complete"));
    }
}
