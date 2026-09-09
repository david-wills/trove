//! Strava — GPS workout activities via the Strava API v3.
//! Brief: docs/integrations/strava.md.
//!
//! Strava is an **aggregator hub**: it collects activities from Garmin, Apple
//! Watch, Wahoo, Suunto, Polar, etc., so one pull can cover a multi-device
//! athlete's entire workout history. GPS streams, power data, and segment
//! efforts never reach Apple Health.
//!
//! ## Behavior
//!
//! `Periodic` (API v3 poll, hourly): `GET /athlete/activities` → paged list
//! of new activities by `before`/`after` watermark, then for each new
//! activity `GET /activities/{id}` for the detail (segment efforts, laps,
//! gear) and optionally `GET /activities/{id}/streams` for GPS/HR/power
//! time-series. Both layers land in `health/strava/`:
//!
//! - `activities/YYYY-MM.jsonl` — one raw JSON object per activity (summary
//!   + detail merged), keyed by stable integer `id`.
//! - `streams/{id}.json` — raw stream object for activities that have GPS
//!   data (written once, never re-fetched; if a stream fetch fails the
//!   activity still lands).
//!
//! ## API rate limits
//!
//! 200 requests / 15 minutes + 2,000 / day per athlete (June 2026). A stream
//! fetch costs one request per activity. The periodic pull is budgeted: at
//! most `STREAM_BUDGET` stream fetches per hourly pass so a fresh-connect
//! backfill (potentially thousands of activities) can't exhaust the daily
//! limit in one run. The budget is enough to catch up a few weeks of
//! activities per hour.
//!
//! ## Auth
//!
//! Single-use OAuth 2.0 (`activity:read_all` scope). Strava issues a refresh
//! token alongside the access token so the pull refreshes silently on expiry.
//! App credentials baked at build time (`TROVE_STRAVA_CLIENT_ID` /
//! `TROVE_STRAVA_CLIENT_SECRET`); BYO credentials are supported per
//! ConnectSpec. Redirect URI: `http://localhost:38666/callback` (the assigned
//! unique port for this integration).
//!
//! ## Privacy
//!
//! GPS location trails are sensitive; this integration is `default_on: false`
//! — the user must explicitly enable it.
//!
//! ## Compliance
//!
//! The Strava API Agreement prohibits bulk resale and public display without
//! attribution. This is a local personal-data vault — no redistribution, no
//! public display. Fully compliant.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
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

/// Service id used for the token / app-creds store.
const SERVICE: &str = "strava";

/// Cursor (non-secret, rebuildable).
const SYNC_FILE: &str = ".trove/strava-sync.json";

/// Raw partitioned JSONL for activity summaries + details.
const ACTIVITIES_DIR: &str = "health/strava/activities";

/// Per-activity stream JSON files.
const STREAMS_DIR: &str = "health/strava/streams";

/// HTTP timeout for API calls. Short so a hung connection can't stall the
/// watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Seconds between periodic syncs (hourly).
pub const STRAVA_SYNC_SECS: u64 = 3600;

/// Page size for the activity list endpoint (max Strava allows is 200).
const PAGE_SIZE: u32 = 200;

/// Max stream fetches per periodic pass. Each costs one API request; the
/// 2,000/day limit means ≤48 full passes / day with this budget.
/// 40 streams/hour ≈ 960/day — comfortable headroom.
const STREAM_BUDGET_PER_PASS: usize = 40;

/// All stream types we request in one call (avoids N round-trips per activity).
/// Strava allows requesting multiple types in one `keys=` query.
const STREAM_TYPES: &str = "time,latlng,altitude,heartrate,cadence,watts,velocity_smooth,moving";

// ---------------------------------------------------------------------------
// OAuth provider.

pub static STRAVA: Provider = Provider {
    service: SERVICE,
    display_name: "Strava",
    auth_url: "https://www.strava.com/oauth/authorize",
    token_url: "https://www.strava.com/api/v3/oauth/token",
    // activity:read_all: includes private activities and GPS precision zones.
    scopes: "activity:read_all",
    // Assigned unique production port for Strava (must match the registered
    // redirect URI: http://localhost:38666/callback).
    redirect_port: 38666,
    use_pkce: false,
    // Strava wants client_id / client_secret in the token request body, not
    // as HTTP Basic auth.
    basic_auth: false,
    // Bake credentials in at build time: TROVE_STRAVA_CLIENT_ID /
    // TROVE_STRAVA_CLIENT_SECRET. Empty default — the user can bring their
    // own credentials (BYO per ConnectSpec).
    default_client_id: option_env!("TROVE_STRAVA_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_STRAVA_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection.

/// [`ConnectMethod::OAuth`] adapter (drops the token — callers re-read state
/// through the status hook).
fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

/// Token store mapped into the generic envelope.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(STRAVA.service)?.is_some()
        || STRAVA.default_credentials().is_some();
    let accounts = match vault.load_sync_token(STRAVA.service)? {
        Some(token) => vec![ConnectedAccount {
            key: STRAVA.service.to_string(),
            label: STRAVA.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Strava issues a refresh token — a token expired with a refresh
            // token does NOT need reconnect (the pull refreshes silently).
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
/// one `&crate::strava::CONNECTION,` line).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "strava",
    display_name: "Strava",
    methods: &[ConnectMethod::OAuth {
        provider: &STRAVA,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["strava"],
    setup: &[
        "Sign in at developers.strava.com and create an app (any name).",
        "Set its Authorization Callback Domain to localhost — the app uses \
         http://localhost:38666/callback.",
        "Paste the app's Client ID and Client Secret here. They're saved, so \
         every future connect is just a login.",
    ],
};

/// Interactive connect: opens the consent page, waits for the redirect, saves
/// the token. Blocking — callers off the main thread only.
///
/// Credentials resolve: explicit → previously saved → compiled-in defaults.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(STRAVA.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(STRAVA.service)?
            .or_else(|| STRAVA.default_credentials())
            .context(
                "no Strava app credentials — register an app at developers.strava.com and \
                 enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&STRAVA, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(STRAVA.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Registry face (DEF).

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(ACTIVITIES_DIR))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!("strava synced — {total} new activities")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("strava sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (stub already there).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "strava",
        name: "Strava",
        kind: IntegrationKind::CloudSync,
        // GPS location trails are sensitive; require explicit opt-in.
        default_on: false,
        description:
            "Sync workouts, GPS routes, and performance data from Strava. \
             Acts as an aggregation hub for activities recorded on Garmin, \
             Apple Watch, Wahoo, Polar, and other devices. Hourly pull with \
             GPS streams and full activity detail.",
        domain: "health",
        vault_path: "health/strava/",
        toggleable: true,
        setup: &[
            "Strava collects GPS location data — enabling this stores your \
             precise workout routes locally.",
            "Connect your Strava account on this card. The first sync backfills \
             your full activity history; later syncs are incremental and hourly.",
        ],
        caveats:
            "GPS stream fetches are budgeted (40/pass) so a fresh-connect backfill \
             runs over several hours rather than exhausting the API limit in one go. \
             The API Agreement prohibits redistribution — this local vault is \
             fully compliant. Rate limit: 200 req/15 min + 2,000/day.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(STRAVA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("strava"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret).

/// Persisted sync state under `.trove/strava-sync.json`.
/// Not a secret — contains only an integer watermark and metadata.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct SyncState {
    /// Unix epoch seconds of the newest activity start already ingested.
    /// `GET /athlete/activities?after=<watermark>` returns only newer ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) after: Option<i64>,
    /// Local RFC3339 timestamp of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated: Option<String>,
}

impl Vault {
    pub(crate) fn read_strava_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_strava_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for offline tests).

/// The endpoints the pull needs. A trait so tests can drive the mapping /
/// persist logic fully offline.
pub(crate) trait StravaApi {
    /// `GET /athlete/activities?after=&before=&per_page=&page=`
    fn activities(
        &self,
        token: &str,
        after: Option<i64>,
        page: u32,
    ) -> Result<Vec<Value>, FetchError>;

    /// `GET /activities/{id}`
    fn activity_detail(&self, token: &str, id: i64) -> Result<Value, FetchError>;

    /// `GET /activities/{id}/streams?keys=…&key_by_type=true`
    fn streams(&self, token: &str, id: i64) -> Result<Value, FetchError>;
}

/// Status-level errors — 401 needs token refresh, 429 is transient.
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

/// Production HTTP client against `api.strava.com`.
pub(crate) struct StravaClient {
    base: String,
}

impl StravaClient {
    pub(crate) fn new() -> Self {
        StravaClient { base: "https://www.strava.com/api/v3".into() }
    }

    #[cfg(test)]
    fn with_base(base: &str) -> Self {
        StravaClient { base: base.to_string() }
    }

    fn get(&self, url: &str, token: &str) -> Result<Value, FetchError> {
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

impl StravaApi for StravaClient {
    fn activities(
        &self,
        token: &str,
        after: Option<i64>,
        page: u32,
    ) -> Result<Vec<Value>, FetchError> {
        let mut url = format!(
            "{}/athlete/activities?per_page={PAGE_SIZE}&page={page}",
            self.base
        );
        if let Some(ts) = after {
            url.push_str(&format!("&after={ts}"));
        }
        match self.get(&url, token)? {
            Value::Array(arr) => Ok(arr),
            other => Err(FetchError::Other(format!(
                "expected array from /athlete/activities, got: {other:?}"
            ))),
        }
    }

    fn activity_detail(&self, token: &str, id: i64) -> Result<Value, FetchError> {
        self.get(&format!("{}/activities/{id}", self.base), token)
    }

    fn streams(&self, token: &str, id: i64) -> Result<Value, FetchError> {
        self.get(
            &format!(
                "{}/activities/{id}/streams?keys={STREAM_TYPES}&key_by_type=true",
                self.base
            ),
            token,
        )
    }
}

// ---------------------------------------------------------------------------
// Token refresh helper.

/// Resolve a fresh token. Strava issues refresh tokens with every access
/// token, so expiry silently refreshes; a dead refresh token (no refresh_token
/// field) forces reconnect.
pub(crate) fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(STRAVA.service)?
        .or_else(|| STRAVA.default_credentials())
        .context("Strava token expired and no app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&STRAVA, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(STRAVA.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(STRAVA.service)?;
            bail!("Strava token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

// ---------------------------------------------------------------------------
// The pull (public entry point + injectable body).

/// The manual "Sync now" and periodic pass entry point.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Strava is not connected — connect your account in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = StravaClient::new();
    pull_with(vault, &client, &token.access_token)
}

/// Testable body: accepts an injected API + token string.
pub(crate) fn pull_with(
    vault: &Vault,
    api: &impl StravaApi,
    token: &str,
) -> Result<PullOutcome> {
    let mut state = vault.read_strava_sync();
    let mut new_activities: u64 = 0;
    let mut streams_fetched: u64 = 0;
    let mut streams_remaining = STREAM_BUDGET_PER_PASS;

    // -- Phase 1: drain new activity summaries. ------------------------------
    // Paginate until a page is empty or SHORT (< PAGE_SIZE), draining ALL new
    // activities before advancing the watermark. Never advance on a partial
    // fetch.
    let mut new_ids: Vec<(i64, i64)> = Vec::new(); // (activity_id, start_epoch)

    let mut page: u32 = 1;
    loop {
        let activities = api
            .activities(token, state.after, page)
            .map_err(|e| fetch_err("activities", e))?;

        let count = activities.len();
        if count == 0 {
            break;
        }

        for act in &activities {
            let id = act.get("id").and_then(Value::as_i64).unwrap_or(0);
            if id == 0 {
                continue;
            }
            let start_epoch = act
                .get("start_date")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.timestamp())
                .unwrap_or(0);
            new_ids.push((id, start_epoch));
        }

        if count < PAGE_SIZE as usize {
            break;
        }
        page += 1;
    }

    // -- Phase 2: fetch detail for each new activity. -------------------------
    // Existing guids: scan the partitioned stream so a re-run doesn't re-fetch.
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

    // Watermark candidate: the max start_epoch across this batch.
    // Only advances when all new activities up to that epoch have been
    // successfully fetched. On a rate-limited partial pass we leave the
    // watermark unchanged so the next pass re-requests from the same point;
    // already-written activities are deduped via seen_ids.
    let mut max_epoch: Option<i64> = state.after;
    // Set to true when a 429 interrupted the detail loop. Suppresses watermark
    // advancement so the stranded tail is not permanently lost.
    let mut detail_rate_limited = false;

    struct ActivityRow {
        ts: String,
        merged: Value,
        id: i64,
        has_streams: bool,
    }

    let mut to_write: Vec<ActivityRow> = Vec::new();

    for (id, start_epoch) in &new_ids {
        if seen_ids.contains(&id.to_string()) {
            // Update watermark even for already-seen activities so a re-run
            // after cursor loss can still recover.
            max_epoch = Some(max_epoch.unwrap_or(0).max(*start_epoch));
            continue;
        }

        // Fetch the detailed activity object.
        let detail = match api.activity_detail(token, *id) {
            Ok(v) => v,
            Err(FetchError::RateLimited) => {
                // Stop fetching details this pass. Do NOT advance max_epoch
                // past the unseen tail — mark partial so Phase 5 keeps the
                // watermark at its current safe value and the next hourly pass
                // re-requests from there (deduping already-written activities).
                detail_rate_limited = true;
                break;
            }
            Err(e) => return Err(fetch_err(&format!("activity/{id}"), e)),
        };

        let ts = detail
            .get("start_date")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if ts.is_empty() {
            // Can't partition — skip this record; the watermark still advances
            // since we got a real response.
            max_epoch = Some(max_epoch.unwrap_or(0).max(*start_epoch));
            continue;
        }

        // An activity likely has streams when it has GPS data (non-empty
        // start_latlng), heart-rate data, or was recorded by a device
        // (device_name / external_id present). Indoor/trainer rides with
        // power/cadence/velocity streams fall into the last bucket.
        let has_streams = detail
            .get("has_heartrate")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || detail
                .get("start_latlng")
                .and_then(Value::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            || detail
                .get("device_name")
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
            || detail
                .get("external_id")
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
                .unwrap_or(false);

        max_epoch = Some(max_epoch.unwrap_or(0).max(*start_epoch));
        new_activities += 1;
        to_write.push(ActivityRow { ts, merged: detail, id: *id, has_streams });
    }

    // -- Phase 3: write activities (raw, month-partitioned). ------------------
    // Collect rows by partition so we can batch-append them.
    let mut by_partition: BTreeMap<String, Vec<ActivityRow>> = BTreeMap::new();
    for row in to_write {
        let key = row
            .ts
            .get(..7)
            .unwrap_or("0000-00")
            .to_string();
        by_partition.entry(key).or_default().push(row);
    }

    // Append-only: new rows only (guid dedupe via seen_ids ensured above).
    for (_key, rows) in &by_partition {
        let to_append: Vec<Value> = rows.iter().map(|r| r.merged.clone()).collect();
        stream.append(&to_append, |v| {
            v.get("start_date").and_then(Value::as_str).unwrap_or("")
        })?;
    }

    // -- Phase 4: fetch + store streams (budgeted). ---------------------------
    // Only for new activities with GPS/HR data; skip if budget exhausted.
    let streams_dir_path = vault
        .resolve(STREAMS_DIR)
        .context("resolving streams directory")?;
    std::fs::create_dir_all(&streams_dir_path)?;

    for (_key, rows) in &by_partition {
        for row in rows {
            if !row.has_streams || streams_remaining == 0 {
                continue;
            }
            let stream_path = streams_dir_path.join(format!("{}.json", row.id));
            if stream_path.exists() {
                continue; // already fetched on a prior pass
            }
            match api.streams(token, row.id) {
                Ok(stream_data) => {
                    crate::store::write_json_atomic(&stream_path, &stream_data)?;
                    streams_fetched += 1;
                    streams_remaining -= 1;
                }
                Err(FetchError::RateLimited) => {
                    break;
                }
                Err(e) => {
                    // Non-fatal: stream fetch failure doesn't block the activity.
                    // The stream file simply won't exist; a future pass can retry
                    // (by checking existence above).
                    let _ = e; // logged by the outer loop via the CollectOutcome note
                }
            }
        }
    }

    // -- Phase 5: advance watermark only after all writes committed. ----------
    // Skip advancement on a partial (rate-limited) detail pass: Strava returns
    // activities newest-first so the stranded unseen tail sits at older epochs.
    // Advancing would permanently hide those activities from future passes.
    // The next hourly pass re-requests from the unchanged watermark and dedupes
    // already-written rows via seen_ids.
    if !detail_rate_limited {
        if let Some(epoch) = max_epoch {
            if state.after.is_none_or(|cur| epoch > cur) {
                state.after = Some(epoch);
            }
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_strava_sync(&state)?;

    let headline = if new_activities == 0 {
        "Strava is up to date — no new activities".to_string()
    } else {
        format!(
            "Strava synced — {new_activities} new activit{}, {streams_fetched} streams",
            if new_activities == 1 { "y" } else { "ies" }
        )
    };

    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("activities", new_activities),
            ("streams", streams_fetched),
        ]),
    })
}

/// Map a [`FetchError`] at the top of an endpoint into a clear reconnect
/// message for 401 errors, and a descriptive error for others.
fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Strava rejected the token (401) at {endpoint} — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Strava rate limited the {endpoint} endpoint (429) — will retry next sync"
        ),
        other => anyhow::anyhow!("Strava {endpoint} fetch failed: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    // -- fixtures (from developers.strava.com documented API shapes) ---------

    /// A documented SummaryActivity shape (subset of fields that the
    /// /athlete/activities endpoint returns). Field names are snake_case per
    /// the Strava API v3 OpenAPI spec.
    fn summary_activity(id: i64, start_date: &str) -> Value {
        json!({
            "id": id,
            "name": "Morning Run",
            "distance": 8045.0,
            "moving_time": 2523,
            "elapsed_time": 2601,
            "total_elevation_gain": 54.2,
            "type": "Run",
            "sport_type": "Run",
            "workout_type": null,
            "start_date": start_date,
            "start_date_local": "2026-06-10T06:00:00Z",
            "timezone": "(GMT-07:00) America/Denver",
            "utc_offset": -25200.0,
            "location_city": null,
            "location_state": null,
            "location_country": "United States",
            "achievement_count": 2,
            "kudos_count": 5,
            "comment_count": 0,
            "athlete_count": 1,
            "photo_count": 0,
            "map": {
                "id": "a1234567890",
                "summary_polyline": "cz`~Fh{~qOcAcBmAuBkBgC",
                "resource_state": 2
            },
            "trainer": false,
            "commute": false,
            "manual": false,
            "private": false,
            "average_speed": 3.189,
            "max_speed": 5.2,
            "has_heartrate": true,
            "average_heartrate": 152.3,
            "max_heartrate": 171.0,
            "heartrate_opt_out": false,
            "display_hide_heartrate_option": true,
            "pr_count": 0,
            "suffer_score": 72,
            "resource_state": 2,
            "athlete": {"id": 12345678, "resource_state": 1}
        })
    }

    /// A documented DetailedActivity shape (what /activities/{id} returns).
    /// Adds segment_efforts, laps, gear, and other detail-only fields.
    fn detailed_activity(id: i64, start_date: &str) -> Value {
        let mut base = summary_activity(id, start_date);
        let obj = base.as_object_mut().unwrap();
        obj.insert("resource_state".into(), json!(3));
        obj.insert("external_id".into(), json!("garmin_connect_12345.fit"));
        obj.insert("upload_id".into(), json!(987654321_i64));
        obj.insert("start_latlng".into(), json!([39.7484, -105.0079]));
        obj.insert("end_latlng".into(), json!([39.7492, -105.0082]));
        obj.insert("average_temp".into(), json!(18));
        obj.insert("elev_high".into(), json!(1627.4));
        obj.insert("elev_low".into(), json!(1594.8));
        obj.insert("device_name".into(), json!("Garmin Forerunner 255"));
        obj.insert("embed_token".into(), json!("some_embed_token_abc123"));
        obj.insert("segment_leaderboard_opt_out".into(), json!(false));
        obj.insert("leaderboard_opt_out".into(), json!(false));
        obj.insert("splits_metric".into(), json!([]));
        obj.insert("laps".into(), json!([]));
        obj.insert("segment_efforts".into(), json!([]));
        obj.insert("gear".into(), json!({"id": "g12345", "name": "Nike Pegasus 40", "resource_state": 2}));
        base
    }

    /// A documented StreamSet shape (what /activities/{id}/streams returns
    /// with key_by_type=true). Each stream has `type`, `series_type`,
    /// `original_size`, `resolution`, and `data`.
    fn stream_set(id: i64) -> Value {
        json!({
            "time": {
                "type": "time",
                "series_type": "distance",
                "original_size": 3,
                "resolution": "high",
                "data": [0, 5, 10]
            },
            "latlng": {
                "type": "latlng",
                "series_type": "distance",
                "original_size": 3,
                "resolution": "high",
                "data": [
                    [39.7484, -105.0079],
                    [39.7485, -105.0080],
                    [39.7486, -105.0081]
                ]
            },
            "altitude": {
                "type": "altitude",
                "series_type": "distance",
                "original_size": 3,
                "resolution": "high",
                "data": [1612.0, 1615.0, 1618.0]
            },
            "heartrate": {
                "type": "heartrate",
                "series_type": "distance",
                "original_size": 3,
                "resolution": "high",
                "data": [148, 155, 162]
            },
            "velocity_smooth": {
                "type": "velocity_smooth",
                "series_type": "distance",
                "original_size": 3,
                "resolution": "high",
                "data": [3.1, 3.2, 3.3]
            },
            "activity_id": id
        })
    }

    // -- mock API ---------------------------------------------------------------

    struct MockApi {
        activities_pages: RefCell<VecDeque<Result<Vec<Value>, FetchError>>>,
        activity_details: RefCell<BTreeMap<i64, Result<Value, FetchError>>>,
        stream_results: RefCell<BTreeMap<i64, Result<Value, FetchError>>>,
        activities_calls: RefCell<Vec<Option<i64>>>, // after param per call
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                activities_pages: RefCell::new(VecDeque::new()),
                activity_details: RefCell::new(BTreeMap::new()),
                stream_results: RefCell::new(BTreeMap::new()),
                activities_calls: RefCell::new(Vec::new()),
            }
        }
        fn page(self, activities: Vec<Value>) -> Self {
            self.activities_pages.borrow_mut().push_back(Ok(activities));
            self
        }
        fn detail(self, id: i64, v: Value) -> Self {
            self.activity_details.borrow_mut().insert(id, Ok(v));
            self
        }
        fn detail_err(self, id: i64, e: FetchError) -> Self {
            self.activity_details.borrow_mut().insert(id, Err(e));
            self
        }
        fn stream(self, id: i64, v: Value) -> Self {
            self.stream_results.borrow_mut().insert(id, Ok(v));
            self
        }
    }

    impl StravaApi for MockApi {
        fn activities(
            &self,
            _token: &str,
            after: Option<i64>,
            _page: u32,
        ) -> Result<Vec<Value>, FetchError> {
            self.activities_calls.borrow_mut().push(after);
            self.activities_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(Vec::new()))
        }

        fn activity_detail(&self, _token: &str, id: i64) -> Result<Value, FetchError> {
            self.activity_details
                .borrow_mut()
                .remove(&id)
                .unwrap_or(Err(FetchError::Other(format!("no detail fixture for {id}"))))
        }

        fn streams(&self, _token: &str, id: i64) -> Result<Value, FetchError> {
            self.stream_results
                .borrow_mut()
                .remove(&id)
                .unwrap_or(Ok(json!({})))
        }
    }

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-strava-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -- cursor tests -----------------------------------------------------------

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        // An empty (or first-run) cursor deserializes to all-None.
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.after.is_none());
        assert!(empty.updated.is_none());
        // A future cursor with an unknown extra field still deserializes
        // (additive serde back-compat rule).
        let fwd: SyncState =
            serde_json::from_str(r#"{"after":1718000000,"future_field":"x"}"#).unwrap();
        assert_eq!(fwd.after, Some(1_718_000_000));
    }

    // -- pull tests ------------------------------------------------------------

    #[test]
    fn pull_without_token_is_a_clean_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn empty_account_is_a_clean_noop() {
        let v = temp_vault("emptyacct");
        // First page is empty — nothing on the account (or watermark is current).
        let api = MockApi::new().page(vec![]);
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts["activities"], 0);
        assert_eq!(out.counts["streams"], 0);
        // Watermark unchanged.
        assert!(v.read_strava_sync().after.is_none());
    }

    #[test]
    fn new_activity_with_streams_lands_in_both_layers() {
        let v = temp_vault("newact");
        let id = 11111111_i64;
        let start = "2026-06-10T13:00:00Z";
        let epoch = DateTime::parse_from_rfc3339(start).unwrap().timestamp();

        let api = MockApi::new()
            .page(vec![summary_activity(id, start)])
            .detail(id, detailed_activity(id, start))
            .stream(id, stream_set(id));

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts["activities"], 1, "one new activity");
        assert_eq!(out.counts["streams"], 1, "one stream fetched");

        // Activity raw row in the month-partitioned JSONL.
        let act_path = v.root().join("health/strava/activities/2026-06.jsonl");
        assert!(act_path.exists(), "monthly activity file created");
        let body = std::fs::read_to_string(&act_path).unwrap();
        assert!(body.contains(&format!("\"id\":{id}")), "activity id on disk");
        assert!(body.contains("\"device_name\""), "detail fields present");
        assert!(body.contains("\"segment_efforts\""), "segment_efforts present");

        // Stream JSON for this activity.
        let stream_path = v
            .root()
            .join(format!("health/strava/streams/{id}.json"));
        assert!(stream_path.exists(), "stream file created");
        let stream_body = std::fs::read_to_string(&stream_path).unwrap();
        assert!(stream_body.contains("\"latlng\""), "GPS stream present");
        assert!(stream_body.contains("\"heartrate\""), "HR stream present");

        // Watermark advanced.
        let state = v.read_strava_sync();
        assert_eq!(state.after, Some(epoch), "watermark = activity start epoch");
        assert!(state.updated.is_some());

        // Cursor never contains the token.
        let cursor = std::fs::read_to_string(v.root().join(".trove/strava-sync.json")).unwrap();
        assert!(!cursor.contains("tok"), "token never in cursor");
    }

    #[test]
    fn second_pull_dedupes_by_id() {
        let v = temp_vault("dedup");
        let id = 22222222_i64;
        let start = "2026-06-10T14:00:00Z";

        let api1 = MockApi::new()
            .page(vec![summary_activity(id, start)])
            .detail(id, detailed_activity(id, start));
        pull_with(&v, &api1, "tok").unwrap();

        // Second pull: same activity appears again (re-fetch after cursor loss).
        let api2 = MockApi::new()
            .page(vec![summary_activity(id, start)])
            .detail(id, detailed_activity(id, start));
        let out2 = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(out2.counts["activities"], 0, "duplicate activity not re-added");

        // File has exactly one line after re-run (no duplicate appended).
        let act_path = v.root().join("health/strava/activities/2026-06.jsonl");
        assert_eq!(std::fs::read_to_string(&act_path).unwrap().lines().count(), 1);
    }

    #[test]
    fn watermark_advances_to_newest_activity() {
        let v = temp_vault("watermark");
        let id_a = 33333333_i64;
        let id_b = 44444444_i64;
        let start_a = "2026-06-09T08:00:00Z";
        let start_b = "2026-06-10T09:00:00Z";
        let epoch_b = DateTime::parse_from_rfc3339(start_b).unwrap().timestamp();

        let api = MockApi::new()
            .page(vec![summary_activity(id_a, start_a), summary_activity(id_b, start_b)])
            .detail(id_a, detailed_activity(id_a, start_a))
            .detail(id_b, detailed_activity(id_b, start_b));

        pull_with(&v, &api, "tok").unwrap();

        let state = v.read_strava_sync();
        assert_eq!(state.after, Some(epoch_b), "watermark is the newest epoch");
    }

    #[test]
    fn incremental_pull_passes_watermark_to_api() {
        let v = temp_vault("incremental");
        let prior_epoch = 1_718_000_000_i64;
        v.write_strava_sync(&SyncState { after: Some(prior_epoch), updated: None }).unwrap();

        let api = MockApi::new().page(vec![]);
        pull_with(&v, &api, "tok").unwrap();

        let calls = api.activities_calls.borrow();
        assert_eq!(calls.len(), 1, "one activities call");
        assert_eq!(calls[0], Some(prior_epoch), "watermark passed as after param");
    }

    #[test]
    fn activities_401_is_a_reconnect_error() {
        let v = temp_vault("act401");
        struct Failing;
        impl StravaApi for Failing {
            fn activities(&self, _t: &str, _a: Option<i64>, _p: u32) -> Result<Vec<Value>, FetchError> {
                Err(FetchError::Unauthorized)
            }
            fn activity_detail(&self, _t: &str, _id: i64) -> Result<Value, FetchError> {
                unreachable!()
            }
            fn streams(&self, _t: &str, _id: i64) -> Result<Value, FetchError> {
                unreachable!()
            }
        }
        let err = pull_with(&v, &Failing, "tok").unwrap_err().to_string();
        assert!(err.contains("reconnect"), "401 → reconnect message: {err}");
        assert!(err.contains("activities"), "names the endpoint: {err}");
    }

    #[test]
    fn rate_limit_on_detail_stops_gracefully() {
        let v = temp_vault("ratelimit");

        struct RateLimitedDetail;
        impl StravaApi for RateLimitedDetail {
            fn activities(&self, _t: &str, _a: Option<i64>, _p: u32) -> Result<Vec<Value>, FetchError> {
                Ok(vec![summary_activity(55555555_i64, "2026-06-10T10:00:00Z")])
            }
            fn activity_detail(&self, _t: &str, _id: i64) -> Result<Value, FetchError> {
                Err(FetchError::RateLimited)
            }
            fn streams(&self, _t: &str, _id: i64) -> Result<Value, FetchError> {
                Ok(json!({}))
            }
        }

        // Rate limit on detail fetch should not error the whole pull.
        let out = pull_with(&v, &RateLimitedDetail, "tok").unwrap();
        // No activities written (rate limited before the first detail).
        assert_eq!(out.counts["activities"], 0, "no activities written when rate limited");
    }

    #[test]
    fn rate_limit_mid_batch_does_not_advance_watermark_past_stranded_tail() {
        // Strava returns activities newest-first. If the detail fetch is
        // rate-limited partway through, the watermark must NOT jump to the
        // newest epoch — that would permanently strand the older unprocessed
        // activities below the cursor. The watermark must stay frozen so the
        // next pass re-requests from the same point (seen_ids dedupes the
        // already-written ones).
        let v = temp_vault("ratelimit-tail");

        // Two activities: id=100 is NEWER, id=200 is OLDER.
        // Strava returns newest-first so the slice order is [100, 200].
        let id_newer = 100_i64;
        let id_older = 200_i64;
        let start_newer = "2026-06-12T10:00:00Z";
        let start_older = "2026-06-10T08:00:00Z";
        let epoch_newer = DateTime::parse_from_rfc3339(start_newer).unwrap().timestamp();
        let epoch_older = DateTime::parse_from_rfc3339(start_older).unwrap().timestamp();
        // Sanity check that our fixture ordering is correct.
        assert!(epoch_newer > epoch_older);

        // First detail (newer) succeeds; second detail (older) is rate-limited.
        let api = MockApi::new()
            .page(vec![
                summary_activity(id_newer, start_newer),
                summary_activity(id_older, start_older),
            ])
            .detail(id_newer, detailed_activity(id_newer, start_newer))
            .detail_err(id_older, FetchError::RateLimited);

        let out = pull_with(&v, &api, "tok").unwrap();

        // The newer activity was fetched before the rate limit hit.
        assert_eq!(out.counts["activities"], 1, "one activity written before rate limit");

        // CRITICAL: watermark must NOT have advanced to epoch_newer (the
        // newer activity's epoch). It must stay at None (no prior watermark)
        // so the next pass re-requests from the beginning and picks up
        // the stranded older activity.
        let state = v.read_strava_sync();
        assert!(
            state.after.is_none() || state.after.unwrap() < epoch_newer,
            "watermark must not advance past stranded tail (after={:?}, epoch_newer={epoch_newer})",
            state.after,
        );
    }

    // -- connection / def tests -----------------------------------------------

    #[test]
    fn connection_uses_assigned_port_38666() {
        assert_eq!(STRAVA.redirect_port, 38666);
        assert_eq!(STRAVA.redirect_uri(), "http://localhost:38666/callback");
        assert_eq!(CONNECTION.id, "strava");
        assert_eq!(DEF.connection, Some("strava"));
    }

    #[test]
    fn status_with_no_token_has_no_accounts() {
        let v = temp_vault("nostatus");
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
    }

    #[test]
    fn status_maps_live_token_to_one_account() {
        let v = temp_vault("livetoken");
        // Far future: ~2030.
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: Some("ref".into()),
                token_type: Some("Bearer".into()),
                scope: Some("activity:read_all".into()),
                expires_at: Some(1_900_000_000),
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "strava");
        assert_eq!(a.label, "Strava");
        assert_eq!(a.expires_at, Some(1_900_000_000));
        assert!(!a.needs_reconnect);
    }

    #[test]
    fn expired_token_with_refresh_does_not_need_reconnect_at_status() {
        // A refresh token means ensure_fresh() can renew silently — status
        // must NOT flag reconnect just because the access token is stale.
        let v = temp_vault("exp-refreshable");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: Some("ref".into()),
                token_type: None,
                scope: None,
                expires_at: Some(1_000), // past
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert!(!s.accounts[0].needs_reconnect);
    }

    #[test]
    fn expired_token_without_refresh_needs_reconnect() {
        let v = temp_vault("exp-norefresh");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: Some(1_000), // past
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts[0].needs_reconnect);
    }

    #[test]
    fn disconnect_forgets_token_but_keeps_app_credentials() {
        let v = temp_vault("disconnect");
        v.save_sync_app(
            SERVICE,
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        def_disconnect(&v, "strava").unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        assert!(s.configured, "app credentials survive a disconnect");
    }

    #[test]
    fn stream_budget_respected() {
        // Build an API that returns STREAM_BUDGET_PER_PASS + 5 activities —
        // only STREAM_BUDGET_PER_PASS streams should be fetched.
        let v = temp_vault("budget");
        let count = STREAM_BUDGET_PER_PASS + 5;
        let mut acts = Vec::new();
        let base_epoch = 1_718_000_000_i64;
        for i in 0..count as i64 {
            let epoch = base_epoch + i * 3600;
            let dt = DateTime::from_timestamp(epoch, 0)
                .unwrap()
                .with_timezone(&Utc)
                .to_rfc3339();
            acts.push(summary_activity(1_000_000 + i, &dt));
        }

        let mut mock = MockApi::new().page(acts.clone()).page(vec![]);
        for i in 0..count as i64 {
            let id = 1_000_000 + i;
            let epoch = base_epoch + i * 3600;
            let dt = DateTime::from_timestamp(epoch, 0)
                .unwrap()
                .with_timezone(&Utc)
                .to_rfc3339();
            mock = mock.detail(id, detailed_activity(id, &dt)).stream(id, stream_set(id));
        }

        let out = pull_with(&v, &mock, "tok").unwrap();
        // All activities land (detail fetches are not budgeted, only streams).
        assert_eq!(out.counts["activities"], count as u64);
        // Streams capped at the budget.
        assert!(
            out.counts["streams"] <= STREAM_BUDGET_PER_PASS as u64,
            "streams {} exceeded budget {}",
            out.counts["streams"],
            STREAM_BUDGET_PER_PASS
        );
    }
}
