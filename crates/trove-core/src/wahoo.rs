//! Wahoo Fitness — GPS workouts via the self-service Wahoo cloud OAuth 2.0 API.
//! Brief: docs/integrations/wahoo.md.
//!
//! Wahoo makes ELEMNT GPS bike computers and KICKR smart trainers. Its
//! documented self-serve API returns workout summaries plus a downloadable FIT
//! file per activity — full GPS, power, and heart-rate streams that don't
//! reach Apple Health. Most Wahoo users auto-sync to Strava, so the Strava
//! integration already captures those activities; this direct pull covers users
//! who skip Strava or want the raw FIT files.
//!
//! ## Behavior
//!
//! `Periodic` (API v1 poll, every 30 minutes): `GET /v1/workouts?page=N&per_page=30`
//! — the Wahoo API sorts workouts by `starts` descending and does NOT support
//! any `order` or `sort` parameters. Incremental sync is driven entirely by
//! `seen_ids` dedup; the pull walks pages newest-first and stops early once a
//! page is fully composed of already-seen ids. For each new workout the pull
//! calls `GET /v1/workouts/{id}` for the full detail including
//! `workout_summary`, then (if the FIT URL is present) downloads the `.fit`
//! binary into `health/wahoo/imports/`. Both layers land in `health/wahoo/`:
//!
//! - `activities/YYYY-MM.jsonl` — one raw JSON object per workout (summary
//!   + detail merged), keyed by stable integer `id`.
//! - `imports/<id>.fit` — the raw FIT file (written once; not re-fetched on
//!   repeat syncs). GPS routes stay embedded in the FIT — the location view
//!   joins them at read time (taxonomy rule, same as Garmin/Strava).
//!
//! ## API rate limits
//!
//! Wahoo does not publish hard rate limits for consumer tokens. The pull
//! pages at 30 workouts per page (the API default) and caps FIT downloads
//! to `FIT_BUDGET_PER_PASS` per run so a fresh-connect backfill over a
//! large history does not stall. Each pass also rescans stored workout
//! records for any whose `.fit` file is missing (e.g. because a previous
//! pass exhausted the budget) and downloads up to the same budget cap.
//!
//! ## Auth
//!
//! OAuth 2.0 at `api.wahooligan.com`, scopes `workouts_read offline_data`.
//! App credentials baked at build time (`TROVE_WAHOO_CLIENT_ID` /
//! `TROVE_WAHOO_CLIENT_SECRET`); BYO credentials per ConnectSpec. Wahoo
//! issues a refresh token alongside the access token (2-hour expiry); tokens
//! refresh silently. Production redirect: `http://localhost:38852/callback`.
//!
//! ## Privacy
//!
//! GPS location trails are sensitive; integration is `default_on: false` —
//! the user must explicitly enable it.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "wahoo";

/// Cursor (non-secret, rebuildable).
const SYNC_FILE: &str = ".trove/wahoo-sync.json";

/// Raw partitioned JSONL for workout summaries + details.
const ACTIVITIES_DIR: &str = "health/wahoo/activities";

/// Downloaded raw FIT files (one per workout).
const IMPORTS_DIR: &str = "health/wahoo/imports";

/// HTTP timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between periodic syncs (every 30 minutes).
pub const WAHOO_SYNC_SECS: u64 = 1800;

/// Page size for the workout list (Wahoo default is 30).
const PAGE_SIZE: u32 = 30;

/// Max FIT file downloads per periodic pass — cap backfill speed.
const FIT_BUDGET_PER_PASS: usize = 50;

// ---------------------------------------------------------------------------
// OAuth provider.

pub static WAHOO: Provider = Provider {
    service: SERVICE,
    display_name: "Wahoo",
    auth_url: "https://api.wahooligan.com/oauth/authorize",
    token_url: "https://api.wahooligan.com/oauth/token",
    scopes: "workouts_read offline_data",
    // Assigned unique production port for Wahoo.
    redirect_port: 38852,
    use_pkce: false,
    // Wahoo expects credentials in the form body, not as Basic auth.
    basic_auth: false,
    default_client_id: option_env!("TROVE_WAHOO_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_WAHOO_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(WAHOO.service)?.is_some()
        || WAHOO.default_credentials().is_some();
    let accounts = match vault.load_sync_token(WAHOO.service)? {
        Some(token) => vec![ConnectedAccount {
            key: WAHOO.service.to_string(),
            label: WAHOO.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            needs_reconnect: token.expired() && token.refresh_token.is_none(),
            extra: BTreeMap::new(),
        }],
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Registered in [`crate::integrations::CONNECTIONS`] (the integrator adds
/// one `&crate::wahoo::CONNECTION,` line).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "wahoo",
    display_name: "Wahoo",
    methods: &[ConnectMethod::OAuth {
        provider: &WAHOO,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["wahoo"],
    setup: &[
        "Register a new app at cloud-api.wahooligan.com (Developer → My Apps). \
         Any name is fine; set the Redirect URI to http://localhost:38852/callback.",
        "Copy the Client ID and Client Secret here. They're saved locally for \
         every future connect.",
        "Heads-up: Wahoo records full GPS traces (location trails). \
         Enabling this integration stores those routes in your vault.",
    ],
};

/// Interactive OAuth connect: opens the consent page, waits for the redirect,
/// saves the token. Blocking — callers off the main thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(WAHOO.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(WAHOO.service)?
            .or_else(|| WAHOO.default_credentials())
            .context(
                "no Wahoo app credentials — register an app at cloud-api.wahooligan.com \
                 and enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&WAHOO, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(WAHOO.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Registry face (DEF).

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(ACTIVITIES_DIR))
}

fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!("wahoo synced — {total} new workouts")
            }))
        }
        Err(e) => {
            Ok(crate::registry::CollectOutcome::note(format!("wahoo sync skipped: {e}")))
        }
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (stub already there).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "wahoo",
        name: "Wahoo Fitness",
        kind: IntegrationKind::CloudSync,
        // GPS location trails are sensitive; require explicit opt-in.
        default_on: false,
        description:
            "Sync cycling and fitness workouts from your Wahoo ELEMNT or KICKR via the \
             Wahoo cloud API, including the raw FIT file per activity (full GPS, power, \
             and heart-rate streams). Periodic pull every 30 minutes.",
        domain: "health",
        vault_path: "health/wahoo/",
        toggleable: true,
        setup: &[
            "Wahoo records GPS location data — enabling this integration stores your \
             precise workout routes locally in your vault.",
            "Connect your Wahoo account on this card. The first sync backfills your \
             full workout history; later syncs are incremental and run every 30 minutes.",
            "Most Wahoo users also auto-sync to Strava; both integrations can run — \
             they store data in separate vault paths and the read view deduplicates \
             by start time.",
        ],
        caveats:
            "FIT file downloads are budgeted (50/pass) so a fresh-connect backfill \
             over a large library runs over several passes rather than all at once; \
             each pass also rescans for any previously-budgeted-out FITs so they \
             are eventually fetched. \
             Token cap: 10 unrevoked tokens per user per app (Wahoo, from 2026).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(WAHOO_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("wahoo"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret).

/// Persisted sync state under `.trove/wahoo-sync.json`.
///
/// The Wahoo API returns workouts sorted by `starts` descending and provides
/// no server-side incremental filter (no `updated_since`, `sort`, or `order`
/// parameters). Incremental sync is driven entirely by `seen_ids` loaded from
/// the partitioned JSONL vault; this struct only records the last-synced
/// timestamp for display purposes.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct SyncState {
    /// Local RFC3339 timestamp of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) synced_at: Option<String>,
}

impl Vault {
    pub(crate) fn read_wahoo_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_wahoo_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for offline tests).

/// The endpoints the pull needs. A trait so tests can drive the list/detail
/// logic fully offline.
pub(crate) trait WahooApi {
    /// `GET /v1/workouts?per_page=N&page=N` — sorted by `starts` descending
    /// (fixed; no `order`/`sort` params accepted by the API).
    fn workouts(&self, token: &str, page: u32) -> Result<Value, FetchError>;

    /// `GET /v1/workouts/{id}`
    fn workout_detail(&self, token: &str, id: i64) -> Result<Value, FetchError>;

    /// Download raw FIT bytes from a URL. Returns the bytes or an error.
    fn download_fit(&self, url: &str) -> Result<Vec<u8>, FetchError>;
}

/// Status-level errors.
#[derive(Debug)]
pub(crate) enum FetchError {
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

/// Production HTTP client against `api.wahooligan.com`.
pub(crate) struct WahooClient {
    base: String,
}

impl WahooClient {
    pub(crate) fn new() -> Self {
        WahooClient { base: "https://api.wahooligan.com".into() }
    }

    #[cfg(test)]
    pub(crate) fn with_base(base: &str) -> Self {
        WahooClient { base: base.to_string() }
    }

    fn get_json(&self, url: &str, token: &str) -> Result<Value, FetchError> {
        match ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json")
            .call()
        {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
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

impl WahooApi for WahooClient {
    fn workouts(&self, token: &str, page: u32) -> Result<Value, FetchError> {
        // The Wahoo API only accepts `page` and `per_page`; `order`/`sort` are
        // not configurable — workouts always come back by `starts` descending.
        let url = format!(
            "{}/v1/workouts?per_page={PAGE_SIZE}&page={page}",
            self.base
        );
        self.get_json(&url, token)
    }

    fn workout_detail(&self, token: &str, id: i64) -> Result<Value, FetchError> {
        let url = format!("{}/v1/workouts/{id}", self.base);
        self.get_json(&url, token)
    }

    fn download_fit(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        match ureq::get(url).timeout(HTTP_TIMEOUT).call() {
            Ok(resp) => {
                let mut bytes = Vec::new();
                use std::io::Read;
                resp.into_reader()
                    .read_to_end(&mut bytes)
                    .map_err(|e| FetchError::Other(format!("reading FIT body: {e}")))?;
                Ok(bytes)
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    &body[..body.len().min(200)]
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Token refresh helper.

pub(crate) fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(WAHOO.service)?
        .or_else(|| WAHOO.default_credentials())
        .context(
            "Wahoo token expired and no app credentials — reconnect from the Integrations tab",
        )?;
    match oauth::refresh_token(&WAHOO, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(WAHOO.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(WAHOO.service)?;
            bail!("Wahoo token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

// ---------------------------------------------------------------------------
// The pull (public entry point + injectable body).

/// The manual "Sync now" and periodic pass entry point.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Wahoo is not connected — connect your account in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = WahooClient::new();
    pull_with(vault, &client, &token.access_token)
}

/// Testable body: accepts an injected API + token string.
pub(crate) fn pull_with(
    vault: &Vault,
    api: &impl WahooApi,
    token: &str,
) -> Result<PullOutcome> {
    let mut state = vault.read_wahoo_sync();
    let mut new_workouts: u64 = 0;
    let mut fits_downloaded: u64 = 0;
    let mut fits_remaining = FIT_BUDGET_PER_PASS;

    let imports_path = vault.resolve(IMPORTS_DIR).context("resolving imports directory")?;
    std::fs::create_dir_all(&imports_path)?;

    // Existing workout ids: scan partitioned streams for dedup.
    // The Wahoo API sorts by `starts` descending and accepts only `page`/`per_page`
    // (no `order`/`sort` params). Incremental sync relies entirely on seen_ids.
    let stream = vault.stream(ACTIVITIES_DIR, Partition::Month);
    let mut seen_ids: HashSet<String> = HashSet::new();
    if let Ok(partitions) = stream.partitions() {
        for key in &partitions {
            if let Ok(records) = stream.read::<Value>(key) {
                for rec in records {
                    if let Some(id) = rec.get("id").and_then(Value::as_i64) {
                        seen_ids.insert(id.to_string());
                    }
                }
            }
        }
    }

    // -- Phase 1: drain new workout list. ------------------------------------
    // Pages are newest-first by `starts`. Stop early once a page is fully
    // composed of already-seen ids — avoids re-paging all history every pass.
    let mut new_ids: Vec<i64> = Vec::new();
    let mut rate_limited = false;

    let mut page: u32 = 1;
    'pager: loop {
        let resp = api
            .workouts(token, page)
            .map_err(|e| fetch_err("workouts", e))?;

        let workouts = resp
            .get("workouts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let count = workouts.len();
        if count == 0 {
            break;
        }

        let mut page_all_seen = true;
        for w in &workouts {
            let id = w.get("id").and_then(Value::as_i64).unwrap_or(0);
            if id == 0 {
                continue;
            }
            if seen_ids.contains(&id.to_string()) {
                // This specific entry is known, but keep scanning the page —
                // there may be newer unseen entries interleaved.
                continue;
            }
            page_all_seen = false;
            new_ids.push(id);
        }

        // If every entry on this page was already stored, older pages will be
        // too — break to avoid re-fetching the entire history on every pass.
        if page_all_seen {
            break 'pager;
        }

        if count < PAGE_SIZE as usize {
            break;
        }
        page += 1;
    }

    // -- Phase 2: fetch detail for each new workout. -------------------------
    struct WorkoutRow {
        ts: String, // starts (for partition)
        merged: Value,
        id: i64,
        fit_url: Option<String>,
    }

    let mut to_write: Vec<WorkoutRow> = Vec::new();

    for id in &new_ids {
        let detail = match api.workout_detail(token, *id) {
            Ok(v) => v,
            Err(FetchError::RateLimited) => {
                rate_limited = true;
                break;
            }
            Err(e) => return Err(fetch_err(&format!("workouts/{id}"), e)),
        };

        // `starts` is the activity timestamp; used for YYYY-MM partitioning.
        let ts = detail
            .get("starts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if ts.is_empty() {
            // Can't partition — skip this workout.
            continue;
        }

        // Extract the FIT file URL from workout_summary.file.url if present.
        let fit_url = detail
            .get("workout_summary")
            .and_then(|ws| ws.get("file"))
            .and_then(|f| f.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string);

        new_workouts += 1;
        to_write.push(WorkoutRow {
            ts,
            merged: detail,
            id: *id,
            fit_url,
        });
    }

    // -- Phase 3: write workout records (raw, month-partitioned). -------------
    // Group by YYYY-MM for the month partition.
    let mut by_partition: BTreeMap<String, Vec<WorkoutRow>> = BTreeMap::new();
    for row in to_write {
        let key = row.ts.get(..7).unwrap_or("0000-00").to_string();
        by_partition.entry(key).or_default().push(row);
    }

    for (_key, rows) in &by_partition {
        let to_append: Vec<Value> = rows.iter().map(|r| r.merged.clone()).collect();
        stream.append(&to_append, |v| {
            v.get("starts").and_then(Value::as_str).unwrap_or("")
        })?;
        // Register new ids so the FIT rescan below can identify them.
        for row in rows {
            seen_ids.insert(row.id.to_string());
        }
    }

    // -- Phase 4: download FIT files (budgeted). -----------------------------
    // 4a. Download FITs for workouts fetched in this pass.
    for (_key, rows) in &by_partition {
        for row in rows {
            let Some(ref url) = row.fit_url else { continue };
            if fits_remaining == 0 {
                break;
            }
            let fit_path = imports_path.join(format!("{}.fit", row.id));
            if fit_path.exists() {
                continue; // already downloaded
            }
            match api.download_fit(url) {
                Ok(bytes) => {
                    crate::store::write_atomic(&fit_path, &bytes)?;
                    fits_downloaded += 1;
                    fits_remaining -= 1;
                }
                Err(FetchError::RateLimited) => break,
                Err(_) => {
                    // Non-fatal: FIT download failure does not block the record.
                    // The .fit file simply won't exist; rescanned next pass.
                }
            }
        }
        if fits_remaining == 0 {
            break;
        }
    }

    // 4b. Rescan all stored workouts for missing FIT files — catches workouts
    //     that were written in previous passes but whose FIT download was
    //     skipped because the budget was exhausted at the time.
    if fits_remaining > 0 && !rate_limited {
        if let Ok(partitions) = stream.partitions() {
            'outer: for key in &partitions {
                if let Ok(records) = stream.read::<Value>(key) {
                    for rec in records {
                        if fits_remaining == 0 {
                            break 'outer;
                        }
                        let Some(id) = rec.get("id").and_then(Value::as_i64) else {
                            continue;
                        };
                        let fit_path = imports_path.join(format!("{id}.fit"));
                        if fit_path.exists() {
                            continue; // already on disk
                        }
                        // Extract FIT URL from the stored record.
                        let fit_url = rec
                            .get("workout_summary")
                            .and_then(|ws| ws.get("file"))
                            .and_then(|f| f.get("url"))
                            .and_then(Value::as_str);
                        let Some(url) = fit_url else { continue };
                        match api.download_fit(url) {
                            Ok(bytes) => {
                                crate::store::write_atomic(&fit_path, &bytes)?;
                                fits_downloaded += 1;
                                fits_remaining -= 1;
                            }
                            Err(FetchError::RateLimited) => break 'outer,
                            Err(_) => {
                                // Non-fatal: retry next pass.
                            }
                        }
                    }
                }
            }
        }
    }

    // -- Phase 5: persist sync state. -----------------------------------------
    // Do not update synced_at if rate-limited mid-detail so the partial set
    // isn't reported as a clean pass.
    if !rate_limited {
        state.synced_at = Some(Local::now().to_rfc3339());
        vault.write_wahoo_sync(&state)?;
    }

    let headline = if new_workouts == 0 && fits_downloaded == 0 {
        "Wahoo is up to date — no new workouts".to_string()
    } else {
        format!(
            "Wahoo synced — {new_workouts} new workout{}, {fits_downloaded} FIT file{} downloaded",
            if new_workouts == 1 { "" } else { "s" },
            if fits_downloaded == 1 { "" } else { "s" },
        )
    };

    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("workouts", new_workouts), ("fit_downloads", fits_downloaded)]),
    })
}

/// Map a [`FetchError`] at an endpoint into a clear reconnect message.
fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Wahoo rejected the token (401) at {endpoint} — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Wahoo rate limited the {endpoint} endpoint (429) — will retry next sync"
        ),
        other => anyhow::anyhow!("Wahoo {endpoint} fetch failed: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    // -- Fixtures from cloud-api.wahooligan.com documented API shapes. --------
    //
    // The Wahoo API returns workouts sorted by `starts` DESCENDING (newest
    // first). There are no `order` or `sort` parameters — the ordering is
    // fixed by the server. Fixtures reflect this real API behaviour.

    /// A workout list response page (GET /v1/workouts).
    /// `workouts` should be ordered newest-`starts`-first, mirroring the real API.
    fn workout_list_page(workouts: Vec<Value>) -> Value {
        // `per_page` in the response is always the requested page size.
        json!({
            "workouts": workouts,
            "total": workouts.len(),
            "page": 1,
            "per_page": PAGE_SIZE
        })
    }

    /// A workout list response page with an explicit per_page (for multi-page
    /// tests where the page is "full" even though we control the count).
    fn workout_list_page_full(workouts: Vec<Value>) -> Value {
        let count = workouts.len();
        json!({
            "workouts": workouts,
            "total": count,
            "page": 1,
            "per_page": PAGE_SIZE
        })
    }

    /// A workout summary object from GET /v1/workouts (list endpoint).
    /// `starts` is the activity start time (ISO 8601 UTC).
    fn workout_summary(id: i64, starts: &str, updated_at: &str) -> Value {
        json!({
            "id": id,
            "starts": starts,
            "minutes": 60,
            "name": "Friday Fun",
            "plan_id": null,
            "plan_ids": [],
            "route_id": null,
            "workout_token": "abc123",
            "workout_type_id": 0,
            "workout_summary": null,
            "created_at": starts,
            "updated_at": updated_at
        })
    }

    /// A workout detail object (GET /v1/workouts/{id}) with
    /// full workout_summary including a FIT file URL.
    fn workout_detail(id: i64, starts: &str, updated_at: &str) -> Value {
        json!({
            "id": id,
            "starts": starts,
            "minutes": 60,
            "name": "Easy Ride",
            "plan_id": null,
            "plan_ids": [],
            "route_id": null,
            "workout_token": "abc123",
            "workout_type_id": 0,
            "workout_summary": {
                "id": 8297,
                "name": "Easy Ride",
                "ascent_accum": "450.00",
                "cadence_avg": "50.00",
                "calories_accum": "1500.00",
                "distance_accum": "24909.71",
                "duration_active_accum": "179.00",
                "duration_paused_accum": "95.25",
                "duration_total_accum": "275.00",
                "heart_rate_avg": "100.00",
                "power_bike_np_last": "150.00",
                "power_bike_tss_last": "304.90",
                "power_avg": "94.59",
                "speed_avg": "10.75",
                "work_accum": "1041480.00",
                "time_zone": "America/Denver",
                "manual": false,
                "edited": false,
                "fitness_app_id": 1002,
                "file": {
                    "url": "https://cdn.wahooligan.com/4_Mile_Segment_.fit"
                },
                "created_at": starts,
                "updated_at": updated_at
            },
            "created_at": starts,
            "updated_at": updated_at
        })
    }

    // -- Mock API. ------------------------------------------------------------
    //
    // The mock is driven by a pre-queued list of page responses (one entry per
    // `workouts()` call). Callers must supply pages in the order they will be
    // consumed (page 1 first). When the queue is exhausted the mock returns an
    // empty page, simulating the end-of-list sentinel.

    struct MockApi {
        list_pages: RefCell<VecDeque<Value>>,
        details: RefCell<BTreeMap<i64, Value>>,
        fit_bytes: Vec<u8>,
    }

    impl MockApi {
        fn new(pages: Vec<Value>, details: BTreeMap<i64, Value>) -> Self {
            MockApi {
                list_pages: RefCell::new(VecDeque::from(pages)),
                details: RefCell::new(details),
                // Minimal valid FIT header bytes (just enough to exercise the
                // download-and-write path without invoking a FIT parser).
                fit_bytes: vec![
                    14, 0x10, 0x49, 0x08, 0, 0, 0, 0, b'.', b'F', b'I', b'T', 0, 0,
                ],
            }
        }
    }

    impl WahooApi for MockApi {
        fn workouts(&self, _token: &str, _page: u32) -> Result<Value, FetchError> {
            // Return the next queued page; empty page when queue is exhausted.
            Ok(self
                .list_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| workout_list_page(vec![])))
        }

        fn workout_detail(&self, _token: &str, id: i64) -> Result<Value, FetchError> {
            self.details
                .borrow()
                .get(&id)
                .cloned()
                .ok_or_else(|| FetchError::Other(format!("no mock detail for id {id}")))
        }

        fn download_fit(&self, _url: &str) -> Result<Vec<u8>, FetchError> {
            Ok(self.fit_bytes.clone())
        }
    }

    // -- Helpers. -------------------------------------------------------------

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-wahoo-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).expect("vault")
    }

    /// Count all records across all partitions.
    fn total_records(vault: &Vault) -> usize {
        let stream = vault.stream(ACTIVITIES_DIR, Partition::Month);
        stream
            .partitions()
            .unwrap_or_default()
            .iter()
            .map(|p| stream.read::<Value>(p).unwrap_or_default().len())
            .sum()
    }

    /// Count FIT files on disk.
    fn count_fits(vault: &Vault) -> usize {
        let p = vault
            .resolve(IMPORTS_DIR)
            .expect("imports resolve");
        if !p.exists() {
            return 0;
        }
        std::fs::read_dir(&p)
            .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| {
                e.path().extension().map(|x| x == "fit").unwrap_or(false)
            }).count())
            .unwrap_or(0)
    }

    // -- Tests. ---------------------------------------------------------------

    #[test]
    fn connection_uses_assigned_port_38852() {
        assert_eq!(WAHOO.redirect_port, 38852);
        assert_eq!(WAHOO.redirect_uri(), "http://localhost:38852/callback");
    }

    #[test]
    fn empty_list_is_up_to_date() {
        let vault = temp_vault("empty");
        let api = MockApi::new(vec![workout_list_page(vec![])], BTreeMap::new());
        let out = pull_with(&vault, &api, "tok").expect("pull");
        assert_eq!(out.counts["workouts"], 0);
        assert!(out.headline.contains("up to date"));
    }

    #[test]
    fn new_workout_is_stored_and_fit_downloaded() {
        let vault = temp_vault("newwkt");

        let id = 56519_i64;
        // starts is the canonical sort key; updated_at may differ.
        let starts = "2015-08-12T09:00:00.000Z";
        let updated = "2018-10-23T20:43:50.000Z";

        let list_page = workout_list_page(vec![workout_summary(id, starts, updated)]);
        let mut details = BTreeMap::new();
        details.insert(id, workout_detail(id, starts, updated));

        let api = MockApi::new(vec![list_page], details);
        let out = pull_with(&vault, &api, "tok").expect("pull");

        assert_eq!(out.counts["workouts"], 1);
        assert_eq!(out.counts["fit_downloads"], 1);
        assert!(out.headline.contains("1 new workout"));

        // Workout record must be in the partitioned JSONL.
        let stream = vault.stream(ACTIVITIES_DIR, Partition::Month);
        let partitions = stream.partitions().expect("partitions");
        assert!(!partitions.is_empty(), "expected at least one partition");

        let records: Vec<Value> = stream.read(&partitions[0]).expect("read");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["id"], json!(56519));
        assert_eq!(records[0]["workout_summary"]["cadence_avg"], json!("50.00"));

        // FIT file must be on disk.
        let fit_path = vault
            .resolve(&format!("{IMPORTS_DIR}/{id}.fit"))
            .expect("resolve");
        assert!(fit_path.exists(), "FIT file should be written");
    }

    #[test]
    fn rerun_does_not_duplicate() {
        let vault = temp_vault("dedup");

        let id = 99_i64;
        let starts = "2026-01-05T10:00:00.000Z";
        let updated = "2026-01-05T12:00:00.000Z";

        let make_api = || {
            let list_page = workout_list_page(vec![workout_summary(id, starts, updated)]);
            let mut details = BTreeMap::new();
            details.insert(id, workout_detail(id, starts, updated));
            MockApi::new(vec![list_page], details)
        };

        let out1 = pull_with(&vault, &make_api(), "tok").expect("first pull");
        assert_eq!(out1.counts["workouts"], 1);

        // Second pull: same list returned (as the real API would for the most
        // recent workout that hasn't been replaced by anything newer).
        let out2 = pull_with(&vault, &make_api(), "tok").expect("second pull");
        assert_eq!(out2.counts["workouts"], 0, "second pull must not duplicate");

        assert_eq!(total_records(&vault), 1);
    }

    /// Multi-page test: verify the pager correctly handles two full pages
    /// (newest-first by starts) plus a partial terminating page.
    ///
    /// Page 1 (newest): ids 61,62,...,60+PAGE_SIZE  (starts 2026-MM-DD newest)
    /// Page 2          : ids 31,...,60
    /// Page 3 (partial): ids  1,...,30  (oldest starts)
    ///
    /// All should land in the vault after two passes.
    #[test]
    fn multi_page_all_workouts_stored() {
        let vault = temp_vault("multipage");

        let n = PAGE_SIZE as i64; // 30

        // Build summaries newest-first (page 1 = ids n*2+1..=n*3 with newest starts).
        let make_summary = |id: i64| -> Value {
            // Assign starts so that higher id = newer, to match descending order.
            let starts = format!("2026-01-{:02}T10:00:00.000Z", id.min(28));
            let upd = format!("2026-06-{:02}T10:00:00.000Z", id.min(28));
            workout_summary(id, &starts, &upd)
        };

        let make_detail = |id: i64| -> (i64, Value) {
            let starts = format!("2026-01-{:02}T10:00:00.000Z", id.min(28));
            let upd = format!("2026-06-{:02}T10:00:00.000Z", id.min(28));
            (id, workout_detail(id, &starts, &upd))
        };

        // Page 1: ids n+1 ..= 2n (newest)
        let page1: Vec<Value> = ((n + 1)..=(2 * n)).rev().map(make_summary).collect();
        // Page 2: ids 1 ..= n (older)
        let page2: Vec<Value> = (1..=n).rev().map(make_summary).collect();

        // page1 has PAGE_SIZE entries → triggers pager to fetch page 2.
        // page2 has PAGE_SIZE entries → triggers page 3.
        // page 3: empty → terminates.
        let mut details: BTreeMap<i64, Value> = BTreeMap::new();
        for id in 1..=(2 * n) {
            let (k, v) = make_detail(id);
            details.insert(k, v);
        }

        let api = MockApi::new(
            vec![
                workout_list_page_full(page1),
                workout_list_page_full(page2),
                // empty terminator supplied implicitly by mock when queue is empty
            ],
            details,
        );

        let out = pull_with(&vault, &api, "tok").expect("pull");
        assert_eq!(out.counts["workouts"], 2 * n as u64, "all workouts must be stored");
        assert_eq!(total_records(&vault), 2 * n as usize);
    }

    /// Incremental sync: workouts older than seen ids stop paging early.
    ///
    /// Pass 1 ingests id=2 (newer starts). Pass 2 presents [id=2(old), id=3(new)]
    /// on page 1 (descending by starts). The pager must NOT stop early on id=2
    /// because id=3 comes after it in the list. But if page 2 would be all-seen,
    /// it should stop there.
    #[test]
    fn incremental_sync_seen_ids_dedup() {
        let vault = temp_vault("incr");

        let id1 = 1_i64;
        let id2 = 2_i64;
        // id2 has a newer starts than id1 (API returns newest first).
        let starts1 = "2025-03-01T08:00:00.000Z";
        let starts2 = "2025-04-01T08:00:00.000Z";
        let upd1 = "2025-03-02T00:00:00.000Z";
        let upd2 = "2025-04-02T00:00:00.000Z";

        // Pass 1: only id2 on the list (newest).
        {
            let list1 = workout_list_page(vec![workout_summary(id2, starts2, upd2)]);
            let mut details1 = BTreeMap::new();
            details1.insert(id2, workout_detail(id2, starts2, upd2));
            let api1 = MockApi::new(vec![list1], details1);
            let out1 = pull_with(&vault, &api1, "tok").expect("first pull");
            assert_eq!(out1.counts["workouts"], 1);
        }

        // Pass 2: list has [id2 (newest, already seen), id1 (older, new)].
        // The real API returns newest-first, so id2 comes before id1.
        {
            let list2 = workout_list_page(vec![
                workout_summary(id2, starts2, upd2), // already seen
                workout_summary(id1, starts1, upd1), // new
            ]);
            let mut details2 = BTreeMap::new();
            details2.insert(id1, workout_detail(id1, starts1, upd1));
            let api2 = MockApi::new(vec![list2], details2);
            let out2 = pull_with(&vault, &api2, "tok").expect("second pull");
            assert_eq!(out2.counts["workouts"], 1, "only id1 is new");
        }

        assert_eq!(total_records(&vault), 2);
    }

    /// Regression: verify the early-break heuristic does NOT skip new workouts
    /// if a page contains a mix of seen and unseen ids.
    #[test]
    fn mixed_page_does_not_skip_new_ids() {
        let vault = temp_vault("mixed");

        let starts_old = "2025-01-01T00:00:00.000Z";
        let starts_new = "2026-01-01T00:00:00.000Z";
        let upd = "2026-01-02T00:00:00.000Z";

        // Pre-load id=1 (old).
        {
            let lp = workout_list_page(vec![workout_summary(1, starts_old, upd)]);
            let mut d = BTreeMap::new();
            d.insert(1i64, workout_detail(1, starts_old, upd));
            let api = MockApi::new(vec![lp], d);
            pull_with(&vault, &api, "tok").expect("seed");
        }

        // Next pass: page contains [id=2(new, newest), id=1(seen, older)].
        {
            let lp = workout_list_page(vec![
                workout_summary(2, starts_new, upd), // new, newest-first
                workout_summary(1, starts_old, upd), // seen
            ]);
            let mut d = BTreeMap::new();
            d.insert(2i64, workout_detail(2, starts_new, upd));
            let api = MockApi::new(vec![lp], d);
            let out = pull_with(&vault, &api, "tok").expect("second pull");
            assert_eq!(out.counts["workouts"], 1, "id=2 must be ingested");
        }

        assert_eq!(total_records(&vault), 2);
    }

    #[test]
    fn fit_url_missing_does_not_block_record() {
        let vault = temp_vault("nofit");

        let id = 77_i64;
        let starts = "2026-02-10T07:30:00.000Z";
        let upd = "2026-02-10T09:00:00.000Z";

        // Workout detail with no file URL (manual workout, file: null).
        let detail_no_fit = json!({
            "id": id,
            "starts": starts,
            "minutes": 45,
            "name": "Manual Workout",
            "plan_id": null,
            "plan_ids": [],
            "route_id": null,
            "workout_token": "xyz",
            "workout_type_id": 0,
            "workout_summary": {
                "id": 1000,
                "name": "Manual",
                "ascent_accum": "0.00",
                "cadence_avg": "0.00",
                "calories_accum": "300.00",
                "distance_accum": "0.00",
                "duration_active_accum": "2700.00",
                "duration_paused_accum": "0.00",
                "duration_total_accum": "2700.00",
                "heart_rate_avg": "120.00",
                "power_bike_np_last": null,
                "power_bike_tss_last": null,
                "power_avg": "0.00",
                "speed_avg": "0.00",
                "work_accum": "0.00",
                "time_zone": "America/Denver",
                "manual": true,
                "edited": false,
                "fitness_app_id": 1002,
                "file": null,
                "created_at": starts,
                "updated_at": upd
            },
            "created_at": starts,
            "updated_at": upd
        });

        let list_page = workout_list_page(vec![workout_summary(id, starts, upd)]);
        let mut details = BTreeMap::new();
        details.insert(id, detail_no_fit);
        let api = MockApi::new(vec![list_page], details);
        let out = pull_with(&vault, &api, "tok").expect("pull");

        assert_eq!(out.counts["workouts"], 1);
        assert_eq!(out.counts["fit_downloads"], 0, "no FIT URL -> no download");

        // Record still stored.
        let stream = vault.stream(ACTIVITIES_DIR, Partition::Month);
        let partitions = stream.partitions().expect("partitions");
        let records: Vec<Value> = stream.read(&partitions[0]).expect("read");
        assert_eq!(records.len(), 1);
    }

    /// FIT budget exhaustion: pass 1 ingests N > FIT_BUDGET_PER_PASS workouts
    /// but can only download FIT_BUDGET_PER_PASS FITs. Pass 2 must complete
    /// the remaining downloads via the rescan path (phase 4b).
    #[test]
    fn fit_budget_rescan_completes_downloads_across_passes() {
        // FIT_BUDGET_PER_PASS = 50. Use 52 workouts spread over 2 list pages
        // (page 1 = 30 items full, page 2 = 22 items partial).
        // Workouts sorted newest-first: ids 52,51,...,1 across the pages.
        let vault = temp_vault("fitbudget");
        let total: usize = FIT_BUDGET_PER_PASS + 2; // 52

        let make_summary = |id: i64| -> Value {
            // Use year+month to avoid exceeding valid day numbers.
            let yr = 2020 + (id / 12);
            let mo = ((id - 1) % 12) + 1;
            let starts = format!("{yr}-{mo:02}-01T10:00:00.000Z");
            let upd = format!("{yr}-{mo:02}-02T10:00:00.000Z");
            workout_summary(id, &starts, &upd)
        };
        let make_detail = |id: i64| -> (i64, Value) {
            let yr = 2020 + (id / 12);
            let mo = ((id - 1) % 12) + 1;
            let starts = format!("{yr}-{mo:02}-01T10:00:00.000Z");
            let upd = format!("{yr}-{mo:02}-02T10:00:00.000Z");
            (id, workout_detail(id, &starts, &upd))
        };

        // ids sorted newest-first: 52, 51, ..., 1
        // Page 1 (full, 30 items): ids 52..=23
        // Page 2 (partial, 22 items): ids 22..=1
        let all_ids_desc: Vec<i64> = (1..=(total as i64)).rev().collect();
        let page1_ids = &all_ids_desc[..PAGE_SIZE as usize];    // ids 52..=23
        let page2_ids = &all_ids_desc[PAGE_SIZE as usize..];    // ids 22..=1

        let page1: Vec<Value> = page1_ids.iter().map(|&id| make_summary(id)).collect();
        let page2: Vec<Value> = page2_ids.iter().map(|&id| make_summary(id)).collect();

        let mut details: BTreeMap<i64, Value> = BTreeMap::new();
        for id in 1..=(total as i64) {
            let (k, v) = make_detail(id);
            details.insert(k, v);
        }

        // Pass 1: drain all 52 workouts across 2 pages + empty terminator.
        // All 52 records written; only 50 FITs downloaded (budget exhausted).
        let api1 = MockApi::new(
            vec![
                workout_list_page_full(page1.clone()),
                workout_list_page(page2),
                // mock returns empty page when queue is empty → terminates pager
            ],
            details.clone(),
        );
        let out1 = pull_with(&vault, &api1, "tok").expect("pass 1");
        assert_eq!(out1.counts["workouts"] as usize, total, "all workouts stored in pass 1");
        assert_eq!(
            out1.counts["fit_downloads"] as usize,
            FIT_BUDGET_PER_PASS,
            "only budget FITs on pass 1"
        );
        assert_eq!(count_fits(&vault), FIT_BUDGET_PER_PASS);

        // Pass 2: page 1 is all-seen → early-break, no new workouts.
        // Rescan (phase 4b) must pick up the 2 missing FITs.
        let api2 = MockApi::new(
            vec![workout_list_page_full(page1)],
            details,
        );
        let out2 = pull_with(&vault, &api2, "tok").expect("pass 2");
        assert_eq!(out2.counts["workouts"], 0, "no new workouts on pass 2");
        assert_eq!(
            out2.counts["fit_downloads"] as usize,
            total - FIT_BUDGET_PER_PASS,
            "remaining FITs must download on pass 2"
        );
        assert_eq!(count_fits(&vault), total, "all FITs on disk after pass 2");
    }

    /// Verify that workouts with a newer updated_at but an old starts
    /// (edited workout) are still correctly handled: they appear in a
    /// newer list position relative to updated_at but the record is
    /// keyed/partitioned by starts. The seen_ids set (not updated_after)
    /// ensures they are not re-ingested.
    #[test]
    fn edited_workout_old_starts_new_updated_at_not_duplicated() {
        let vault = temp_vault("edited");

        let id = 42_i64;
        let starts = "2024-03-15T09:00:00.000Z"; // old ride
        let upd_v1 = "2024-03-15T10:00:00.000Z"; // original sync time
        let upd_v2 = "2026-06-01T12:00:00.000Z"; // edited later

        // Pass 1: ingest the workout at v1.
        {
            let lp = workout_list_page(vec![workout_summary(id, starts, upd_v1)]);
            let mut d = BTreeMap::new();
            d.insert(id, workout_detail(id, starts, upd_v1));
            let api = MockApi::new(vec![lp], d);
            let out = pull_with(&vault, &api, "tok").expect("pass 1");
            assert_eq!(out.counts["workouts"], 1);
        }

        // Pass 2: same workout reappears with new updated_at (edited).
        // With a watermark-based cursor this would have been re-ingested;
        // seen_ids correctly suppresses it.
        {
            let lp = workout_list_page(vec![workout_summary(id, starts, upd_v2)]);
            let mut d = BTreeMap::new();
            d.insert(id, workout_detail(id, starts, upd_v2));
            let api = MockApi::new(vec![lp], d);
            let out = pull_with(&vault, &api, "tok").expect("pass 2");
            assert_eq!(out.counts["workouts"], 0, "edited workout must not be re-ingested");
        }

        assert_eq!(total_records(&vault), 1, "exactly one record in vault");
    }
}
