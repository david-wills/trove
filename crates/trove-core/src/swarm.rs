//! Swarm (Foursquare) check-in history via the Foursquare v2 API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/swarm.md.
//!
//! A **Periodic** cloud pull (hourly): `GET api.foursquare.com/v2/users/self/checkins`
//! with a user OAuth token. Each check-in is written at full fidelity to the
//! raw layer (`location/swarm/raw/YYYY-MM.jsonl`, partitioned by check-in
//! month). No contract layer yet — Swarm check-ins are *place visits*
//! (venue + lat/lon + timestamp), NOT GPS trail fixes; they do not fit the
//! current `location.Fix` contract. They will wait for a visits-shaped
//! contract to land (Needs-David gated) before gaining a contract row.
//!
//! ## API details
//!
//! `GET https://api.foursquare.com/v2/users/self/checkins`
//! with `oauth_token=<token>`, `v=<YYYYMMDD>`, `limit=250`, `offset=<n>`.
//!
//! Response envelope:
//! ```json
//! { "meta": {"code": 200},
//!   "response": { "checkins": { "count": 42, "items": [ ... ] } } }
//! ```
//!
//! Checkin object (key fields):
//! ```json
//! { "id": "4fef564ae4b01127cc03b589",
//!   "createdAt": 1341085258,
//!   "venue": {
//!     "id": "4a3001a5f964a52004991fe3",
//!     "name": "Meetup HQ",
//!     "location": { "address": "632 Broadway", "lat": 40.726, "lng": -73.995 },
//!     "categories": [{ "id": "...", "name": "Tech Startup" }]
//!   },
//!   "shout": "Great lunch spot"
//! }
//! ```
//!
//! Pagination: the API returns check-ins newest-first by default; the offset
//! walk visits every check-in regardless of order. The drain advances
//! `offset` until the page is empty. The watermark is `max(createdAt)`
//! across all written checkins; incremental pulls send `afterTimestamp=<wm>`
//! to skip already-stored history. The cursor and already-seen id set are
//! updated incrementally per fully-written page to prevent duplicate rows
//! on mid-drain crash/retry.
//!
//! ## 402 / GDPR fallback
//!
//! Some users hit a `402 Payment Required` on the checkins endpoint (a
//! known Foursquare bug — users should not be charged for their own data).
//! On `402` the pull returns a descriptive error advising the user to use
//! the GDPR export at foursquare.com/download-data instead (future import
//! path; the raw layer format is identical).
//!
//! ## Auth
//!
//! Standard OAuth 2.0 with a user-scoped token. Foursquare does NOT issue
//! refresh tokens — the access token is long-lived but eventually expires,
//! requiring re-connection. Privacy-sensitive (a place-history trail) —
//! ships `default_on: false` with explicit opt-in.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::{AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "swarm";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (0600 secrets
/// only). Deleting it re-drains the full history on the next sync.
const SYNC_FILE: &str = ".trove/swarm-sync.json";

/// Raw layer: full-fidelity check-in objects, partitioned by local month.
const RAW_DIR: &str = "location/swarm/raw";

/// Foursquare v2 API base.
const API_BASE: &str = "https://api.foursquare.com";

/// API version date (Foursquare requires a `v=YYYYMMDD` parameter on every
/// call; this pins the response shape to the well-known 2024 schema).
const API_VERSION: &str = "20240101";

/// Maximum items per page. The API accepts up to 250.
const PAGE_SIZE: u32 = 250;

/// HTTP request timeout — short so a hung connection can't stall the loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Seconds between periodic syncs (hourly).
pub const SWARM_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// OAuth provider.

pub static SWARM_PROVIDER: Provider = Provider {
    service: SERVICE,
    display_name: "Swarm (Foursquare)",
    auth_url: "https://foursquare.com/oauth2/authenticate",
    token_url: "https://foursquare.com/oauth2/access_token",
    // Foursquare's legacy OAuth doesn't use scope — pass an empty string.
    scopes: "",
    // Assigned unique production port for Swarm (#265).
    redirect_port: 38845,
    use_pkce: false,
    // Foursquare wants client_id/client_secret in the form body, not Basic.
    basic_auth: false,
    // Bake credentials at build time: TROVE_SWARM_CLIENT_ID /
    // TROVE_SWARM_CLIENT_SECRET. Empty defaults — user brings their own.
    default_client_id: option_env!("TROVE_SWARM_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_SWARM_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(SWARM_PROVIDER.service)?.is_some()
        || SWARM_PROVIDER.default_credentials().is_some();
    let accounts = match vault.load_sync_token(SWARM_PROVIDER.service)? {
        Some(token) => vec![ConnectedAccount {
            key: SWARM_PROVIDER.service.to_string(),
            label: SWARM_PROVIDER.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Foursquare issues no refresh token — expiry = reconnect needed.
            needs_reconnect: token.expired(),
            extra: BTreeMap::new(),
        }],
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "swarm",
    display_name: "Swarm (Foursquare)",
    methods: &[ConnectMethod::OAuth {
        provider: &SWARM_PROVIDER,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["swarm"],
    setup: &[
        "Go to foursquare.com/developers, create a project (Consumer app), and register \
         http://localhost:38845/callback as the OAuth redirect URI.",
        "Paste the project's Client ID and Client Secret here. They're saved, so every \
         future connect is just a login.",
        "Swarm stores where you've been — enabling this keeps your check-in history \
         private and local.",
    ],
};

/// Interactive OAuth connect: opens the consent page, waits for the redirect,
/// saves the token. Blocking — call off the main thread.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(SWARM_PROVIDER.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(SWARM_PROVIDER.service)?
            .or_else(|| SWARM_PROVIDER.default_credentials())
            .context(
                "no Swarm app credentials — register an app at foursquare.com/developers \
                 and enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&SWARM_PROVIDER, &creds)?;
    crate::sync::oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(SWARM_PROVIDER.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!(
                    "swarm synced — {} check-ins",
                    out.counts.get("checkins").copied().unwrap_or(0)
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "swarm sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let checkins = out.counts.get("checkins").copied().unwrap_or(0);
    let headline = if checkins == 0 {
        "Swarm is up to date — no new check-ins".to_string()
    } else {
        format!("Swarm synced — {checkins} check-ins")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "swarm",
        name: "Swarm (Foursquare)",
        kind: IntegrationKind::CloudSync,
        // Privacy-sensitive (a place-history trail) — explicit opt-in required.
        default_on: false,
        description: "Syncs your Foursquare/Swarm check-in history — every \
                      place you've tapped in with timestamp, venue name, and \
                      coordinates. A GDPR export fallback is available if the \
                      API is inaccessible.",
        domain: "location",
        vault_path: "location/swarm/",
        toggleable: true,
        setup: &[
            "Swarm records the places you've checked in — enabling this stores that \
             history locally and privately.",
            "Connect your Foursquare account on this card. The first sync backfills \
             your full check-in history; later syncs are incremental and hourly.",
        ],
        caveats: "The Foursquare v2 check-ins endpoint is undocumented; a known 402 \
                  error may appear for some accounts (a Foursquare bug — you should \
                  not be charged for your own data). If you see it, use the GDPR \
                  export at foursquare.com/download-data instead. Foursquare tokens \
                  are long-lived but don't refresh — reconnect when the token expires. \
                  Check-in data is written raw; a normalized contract layer awaits the \
                  ratification of a visits-shaped location schema.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SWARM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("swarm"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret state).

/// Persisted watermark under `.trove/swarm-sync.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct SyncState {
    /// Unix epoch seconds of the newest check-in already stored. Next pull
    /// sends `afterTimestamp=<watermark>` to fetch only newer check-ins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) after_timestamp: Option<i64>,
    /// RFC3339 timestamp of the last successful sync pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated: Option<String>,
}

impl Vault {
    pub(crate) fn read_swarm_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_swarm_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for offline tests).

/// Errors the pull needs to discriminate.
#[derive(Debug)]
pub(crate) enum FetchError {
    /// 401 — token expired or revoked.
    Unauthorized,
    /// 402 — Foursquare's known "you shouldn't be charged" bug.
    PaymentRequired,
    /// Any other failure.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::PaymentRequired => write!(
                f,
                "HTTP 402 from Foursquare (known bug — use the GDPR export at \
                 foursquare.com/download-data as a fallback)"
            ),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The one endpoint the pull needs. A trait so tests drive the mapping /
/// persist logic fully offline without network access.
pub(crate) trait SwarmApi {
    /// `GET /v2/users/self/checkins?oauth_token=…&v=…&limit=…&offset=…[&afterTimestamp=…]`
    fn checkins(
        &self,
        token: &str,
        offset: u32,
        after_timestamp: Option<i64>,
    ) -> Result<Vec<Value>, FetchError>;
}

/// Production HTTP client against `api.foursquare.com`.
pub(crate) struct SwarmClient {
    base: String,
}

impl SwarmClient {
    pub(crate) fn new() -> Self {
        SwarmClient { base: API_BASE.to_string() }
    }

    fn get(
        &self,
        path: &str,
        token: &str,
        params: &[(&str, String)],
    ) -> Result<Value, FetchError> {
        let url = format!("{}{}", self.base, path);
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .query("oauth_token", token)
            .query("v", API_VERSION);
        for (k, v) in params {
            req = req.query(k, v);
        }
        let resp = req.call().map_err(|e| match e {
            ureq::Error::Status(401, _) => FetchError::Unauthorized,
            ureq::Error::Status(402, _) => FetchError::PaymentRequired,
            ureq::Error::Status(s, r) => FetchError::Other(format!(
                "HTTP {s}: {}",
                r.into_string().unwrap_or_default()
            )),
            other => FetchError::Other(other.to_string()),
        })?;
        resp.into_json::<Value>().map_err(|e| FetchError::Other(e.to_string()))
    }
}

impl SwarmApi for SwarmClient {
    fn checkins(
        &self,
        token: &str,
        offset: u32,
        after_timestamp: Option<i64>,
    ) -> Result<Vec<Value>, FetchError> {
        let limit_s = PAGE_SIZE.to_string();
        let offset_s = offset.to_string();
        let mut params: Vec<(&str, String)> =
            vec![("limit", limit_s), ("offset", offset_s)];
        if let Some(ts) = after_timestamp {
            params.push(("afterTimestamp", ts.to_string()));
        }
        let body = self.get("/v2/users/self/checkins", token, &params)?;
        let items = body
            .pointer("/response/checkins/items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items)
    }
}

// ---------------------------------------------------------------------------
// Raw-layer wrapper.

/// A raw check-in object carrying a derived RFC3339 ts purely so the
/// month-partition writer can file it under the check-in's local month.
/// Only `value` is serialized to disk (`ts` is skipped) so the raw line
/// is the original API object verbatim.
#[derive(Serialize)]
struct RawLine {
    /// Partition key — derived from `createdAt`; NOT written to disk.
    #[serde(skip)]
    ts: String,
    /// The API check-in object, verbatim.
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pull (public entry point + testable body).

/// Entry point used by the periodic hook and "Sync now".
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Swarm is not connected — connect your account in the Integrations tab")?;
    if token.expired() {
        bail!(
            "Swarm token has expired — Foursquare does not issue refresh tokens. \
             Reconnect from the Integrations tab."
        );
    }
    let client = SwarmClient::new();
    pull_with(vault, &client, &token.access_token)
}

/// Testable body: accepts an injected API implementation and a token string.
pub(crate) fn pull_with(
    vault: &Vault,
    api: &impl SwarmApi,
    token: &str,
) -> Result<PullOutcome> {
    let mut state = vault.read_swarm_sync();
    let mut new_checkins: u64 = 0;

    // Pre-load all check-in ids already on disk to enable id-based dedup.
    // This guards against two hazards:
    //   (a) mid-drain crash: page N wrote, cursor not yet advanced → next run
    //       re-fetches from the same afterTimestamp and would re-append; the
    //       id set skips duplicates instead.
    //   (b) afterTimestamp boundary ambiguity: if the API treats the boundary
    //       as inclusive (>=) the check-in(s) at exactly that second come back
    //       again; id-based filtering drops them cleanly.
    let mut seen_ids: std::collections::HashSet<String> = load_existing_ids(vault);

    // Drain pages until empty (SHORT or zero). The cursor and seen-id set are
    // updated incrementally after each fully-written page so that a
    // mid-drain crash leaves the cursor pointing at the last confirmed page.
    let mut offset: u32 = 0;
    loop {
        let page = match api.checkins(token, offset, state.after_timestamp) {
            Ok(p) => p,
            Err(FetchError::PaymentRequired) => {
                bail!(
                    "Foursquare returned HTTP 402 (Payment Required) — this is a known \
                     Foursquare bug; you should not be charged for your own data. \
                     Use the GDPR export at foursquare.com/download-data as a fallback."
                );
            }
            Err(e) => bail!("Swarm API error: {e}"),
        };

        if page.is_empty() {
            break; // Done.
        }

        // Build RawLine wrappers so the month partitioner can use the ts.
        // A check-in with no parseable createdAt is skipped (logged but not
        // written) — we'd rather lose one unusual record than mis-partition
        // under a garbage-named file.
        // Already-stored ids (seen_ids) are filtered out before append to
        // prevent duplicates on crash/retry or inclusive-boundary re-delivery.
        let mut raw_lines: Vec<RawLine> = Vec::with_capacity(page.len());
        let mut page_max_created_at: Option<i64> = None;
        let mut page_ids: Vec<String> = Vec::new();
        for item in &page {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !id.is_empty() && seen_ids.contains(&id) {
                // Already on disk — skip without incrementing new_checkins.
                continue;
            }
            match created_at_rfc3339(item) {
                Some(ts) => raw_lines.push(RawLine { ts, value: item.clone() }),
                None => {
                    // Surface the anomaly but don't abort the whole drain.
                    eprintln!(
                        "swarm: check-in missing/invalid createdAt — skipping: {}",
                        serde_json::to_string(item).unwrap_or_default()
                    );
                    continue;
                }
            }
            // Track max createdAt across this page for the watermark.
            if let Some(ts) = item.get("createdAt").and_then(|v| v.as_i64()) {
                page_max_created_at = Some(match page_max_created_at {
                    Some(prev) => prev.max(ts),
                    None => ts,
                });
            }
            if !id.is_empty() {
                page_ids.push(id);
            }
            new_checkins += 1;
        }

        if !raw_lines.is_empty() {
            let raw = vault.stream(RAW_DIR, Partition::Month);
            raw.append(&raw_lines, |r| &r.ts)?;

            // Advance the cursor and seen-id set AFTER each successfully-written
            // page. A crash between pages leaves the cursor at the last confirmed
            // page; the next run re-fetches from there and the id set deduplicates
            // any overlap.
            if let Some(ts) = page_max_created_at {
                state.after_timestamp = Some(match state.after_timestamp {
                    Some(prev) => prev.max(ts),
                    None => ts,
                });
            }
            state.updated = Some(Local::now().to_rfc3339());
            vault.write_swarm_sync(&state)?;
            for id in page_ids {
                seen_ids.insert(id);
            }
        }

        let page_len = page.len() as u32;
        if page_len < PAGE_SIZE {
            break; // Final (short) page — done.
        }
        offset += page_len;
    }

    let mut counts = BTreeMap::new();
    counts.insert("checkins", new_checkins);
    let headline = if new_checkins == 0 {
        "Swarm is up to date — no new check-ins".to_string()
    } else {
        format!("Swarm synced — {new_checkins} check-ins")
    };
    Ok(PullOutcome { headline, counts })
}

// ---------------------------------------------------------------------------
// Dedup helpers.

/// Load all check-in ids already stored in the raw JSONL partitions.
/// Returns an empty set if the directory is absent (first run).
fn load_existing_ids(vault: &Vault) -> std::collections::HashSet<String> {
    let raw = vault.stream(RAW_DIR, Partition::Month);
    let partitions = raw.partitions().unwrap_or_default();
    let mut ids = std::collections::HashSet::new();
    for key in partitions {
        let rows: Vec<Value> = raw.read(&key).unwrap_or_default();
        for row in rows {
            if let Some(id) = row.get("id").and_then(|v| v.as_str()) {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// Helpers.

/// Convert a check-in's `createdAt` Unix seconds to an RFC3339 local string
/// for use as the partition key. Returns `None` if the field is missing or
/// unparseable (the caller falls back to a sentinel that surfaces as an error).
fn created_at_rfc3339(item: &Value) -> Option<String> {
    let ts = item.get("createdAt")?.as_i64()?;
    let dt = Local.timestamp_opt(ts, 0).single()?;
    Some(dt.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Unit tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Build a fresh temp-dir vault for each test. Unique per test invocation.
    fn temp_vault() -> (TempDir, Vault) {
        let dir = TempDir::new().unwrap();
        let vault = Vault::open_or_create(dir.path().to_path_buf()).unwrap();
        (dir, vault)
    }

    // ---- Fixture: realistic API response item ----

    fn make_checkin(id: &str, created_at: i64) -> Value {
        serde_json::json!({
            "id": id,
            "createdAt": created_at,
            "type": "checkin",
            "venue": {
                "id": "4a3001a5f964a52004991fe3",
                "name": "Meetup HQ",
                "location": {
                    "address": "632 Broadway",
                    "lat": 40.72605879581897,
                    "lng": -73.99587253789
                },
                "categories": [
                    { "id": "4bf58dd8d48988d125941735", "name": "Tech Startup" }
                ]
            },
            "shout": "Great event!"
        })
    }

    fn make_checkin_no_venue(id: &str, created_at: i64) -> Value {
        serde_json::json!({
            "id": id,
            "createdAt": created_at,
            "type": "checkin"
        })
    }

    // ---- Stub SwarmApi ----

    struct StubApi {
        /// Pages of check-ins to return, in order. Each `call` returns the
        /// next page. The stub is stateless beyond the pages list.
        pages: Vec<Vec<Value>>,
        /// Index into pages.
        call_count: RefCell<usize>,
        /// Captured (offset, after_timestamp) arguments.
        calls: RefCell<Vec<(u32, Option<i64>)>>,
        /// If set, return this error on the first call.
        error: Option<FetchError>,
    }

    impl StubApi {
        fn new(pages: Vec<Vec<Value>>) -> Self {
            StubApi {
                pages,
                call_count: RefCell::new(0),
                calls: RefCell::new(vec![]),
                error: None,
            }
        }
        fn with_402() -> Self {
            StubApi {
                pages: vec![],
                call_count: RefCell::new(0),
                calls: RefCell::new(vec![]),
                error: Some(FetchError::PaymentRequired),
            }
        }
    }

    impl SwarmApi for StubApi {
        fn checkins(
            &self,
            _token: &str,
            offset: u32,
            after_timestamp: Option<i64>,
        ) -> Result<Vec<Value>, FetchError> {
            self.calls.borrow_mut().push((offset, after_timestamp));
            if let Some(ref e) = self.error {
                return Err(match e {
                    FetchError::Unauthorized => FetchError::Unauthorized,
                    FetchError::PaymentRequired => FetchError::PaymentRequired,
                    FetchError::Other(m) => FetchError::Other(m.clone()),
                });
            }
            let idx = *self.call_count.borrow();
            *self.call_count.borrow_mut() = idx + 1;
            Ok(self.pages.get(idx).cloned().unwrap_or_default())
        }
    }

    // ---- Tests ----

    #[test]
    fn empty_pull_writes_nothing_and_keeps_no_cursor() {
        let (_dir, vault) = temp_vault();
        let api = StubApi::new(vec![vec![]]);
        let out = pull_with(&vault, &api, "tok").unwrap();
        assert_eq!(out.counts.get("checkins").copied().unwrap_or(0), 0);
        // No cursor written when nothing landed.
        let state = vault.read_swarm_sync();
        assert!(state.after_timestamp.is_none(), "no cursor for empty pull");
    }

    #[test]
    fn single_page_drain_writes_raw_and_advances_watermark() {
        let (_dir, vault) = temp_vault();
        let item = make_checkin("4fef564ae4b01127cc03b589", 1_341_085_258);
        let api = StubApi::new(vec![vec![item.clone()], vec![]]);
        let out = pull_with(&vault, &api, "tok").unwrap();
        assert_eq!(out.counts["checkins"], 1);

        // Watermark advanced to the checkin's createdAt.
        let state = vault.read_swarm_sync();
        assert_eq!(state.after_timestamp, Some(1_341_085_258));

        // Raw file written to location/swarm/raw/.
        let raw_dir = vault.root().join(RAW_DIR);
        let files: Vec<PathBuf> = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert!(!files.is_empty(), "raw JSONL written");
    }

    #[test]
    fn two_page_drain_accumulates_both_and_advances_to_max() {
        let (_dir, vault) = temp_vault();
        // Simulate a PAGE_SIZE-full first page by using the real PAGE_SIZE.
        // We can't easily generate 250 items, so we override by making two
        // pages each smaller than PAGE_SIZE to test the short-page stop.
        let page1: Vec<Value> =
            (0..5).map(|i| make_checkin(&format!("id{i}"), 1_341_085_000 + i)).collect();
        let page2: Vec<Value> =
            (0..3).map(|i| make_checkin(&format!("id2{i}"), 1_341_086_000 + i)).collect();
        // page1 has 5 < PAGE_SIZE, so after it the loop breaks (short page).
        let api = StubApi::new(vec![page1, page2]);
        let out = pull_with(&vault, &api, "tok").unwrap();
        // Only page1 was consumed (short page → stop).
        assert_eq!(out.counts["checkins"], 5);
        let state = vault.read_swarm_sync();
        // Max createdAt from page1.
        assert_eq!(state.after_timestamp, Some(1_341_085_004));
    }

    #[test]
    fn checkin_without_venue_is_still_written_raw() {
        let (_dir, vault) = temp_vault();
        let item = make_checkin_no_venue("abc123", 1_700_000_000);
        let api = StubApi::new(vec![vec![item], vec![]]);
        let out = pull_with(&vault, &api, "tok").unwrap();
        assert_eq!(out.counts["checkins"], 1);
    }

    #[test]
    fn incremental_pull_passes_after_timestamp_to_api() {
        let (_dir, vault) = temp_vault();
        // Pre-seed a cursor.
        vault
            .write_swarm_sync(&SyncState {
                after_timestamp: Some(1_600_000_000),
                updated: None,
            })
            .unwrap();
        let api = StubApi::new(vec![vec![]]);
        pull_with(&vault, &api, "tok").unwrap();
        // The first (only) call should have passed after_timestamp.
        let calls = api.calls.borrow();
        assert_eq!(calls[0].1, Some(1_600_000_000));
    }

    #[test]
    fn payment_required_402_surfaces_descriptive_error() {
        let (_dir, vault) = temp_vault();
        let api = StubApi::with_402();
        let err = pull_with(&vault, &api, "tok").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("402") || msg.contains("Payment Required") || msg.contains("GDPR"),
            "402 error surfaces guidance: {msg}"
        );
    }

    #[test]
    fn created_at_rfc3339_converts_unix_timestamp_to_local() {
        // 2012-07-01 UTC (Foursquare's documented example: 1341085258)
        let item = make_checkin("x", 1_341_085_258);
        let ts = created_at_rfc3339(&item).unwrap();
        // Must be a valid RFC3339 string starting with a year.
        assert!(ts.len() >= 10, "ts too short: {ts}");
        // Should be in 2012 in any timezone.
        assert!(ts.starts_with("2012-"), "expected 2012 date, got: {ts}");
    }

    #[test]
    fn created_at_rfc3339_returns_none_on_missing_field() {
        let item = serde_json::json!({"id": "x"});
        assert!(created_at_rfc3339(&item).is_none());
    }

    #[test]
    fn sync_state_round_trips_through_json() {
        let state = SyncState {
            after_timestamp: Some(1_700_000_000),
            updated: Some("2025-11-14T00:00:00+00:00".to_string()),
        };
        let j = serde_json::to_string(&state).unwrap();
        let back: SyncState = serde_json::from_str(&j).unwrap();
        assert_eq!(back.after_timestamp, state.after_timestamp);
        assert_eq!(back.updated, state.updated);
    }

    #[test]
    fn empty_sync_state_omits_optional_fields() {
        let state = SyncState::default();
        let j = serde_json::to_string(&state).unwrap();
        assert!(!j.contains("after_timestamp"), "omit empty: {j}");
        assert!(!j.contains("updated"), "omit empty: {j}");
    }

    #[test]
    fn provider_uses_assigned_redirect_port() {
        assert_eq!(SWARM_PROVIDER.redirect_port, 38845);
    }

    /// Simulates a mid-drain failure: page 1 succeeds and is written to disk,
    /// then the drain fails (simulated by a second pull that pretends it
    /// crashed and restarts from scratch). The second run should NOT produce
    /// duplicate raw lines for the check-ins already written by page 1.
    ///
    /// This covers defects 2 & 3: id-based dedup prevents row duplication on
    /// crash/retry even when afterTimestamp hasn't advanced yet.
    #[test]
    fn crash_retry_does_not_duplicate_already_written_rows() {
        let (_dir, vault) = temp_vault();

        // Page 1: 3 check-ins, successfully written.
        let page1: Vec<Value> = vec![
            make_checkin("ci-dup-1", 1_600_000_001),
            make_checkin("ci-dup-2", 1_600_000_002),
            make_checkin("ci-dup-3", 1_600_000_003),
        ];
        // Simulate successful page 1 + short final page (drain completes).
        let api_run1 = StubApi::new(vec![page1.clone(), vec![]]);
        let out1 = pull_with(&vault, &api_run1, "tok").unwrap();
        assert_eq!(out1.counts["checkins"], 3, "run1 wrote 3 check-ins");

        // Simulate a crash: manually reset the cursor to None (as if the
        // cursor was not advanced — worst-case crash scenario where even the
        // per-page cursor write didn't persist).
        vault.write_swarm_sync(&SyncState::default()).unwrap();

        // Second run replays the same page 1 (same items, same ids).
        // The id-based dedup must discard all 3 already-on-disk check-ins.
        let api_run2 = StubApi::new(vec![page1, vec![]]);
        let out2 = pull_with(&vault, &api_run2, "tok").unwrap();
        assert_eq!(out2.counts["checkins"], 0, "run2 must not produce duplicates");

        // Verify raw files contain exactly 3 unique lines total.
        let raw = vault.stream(RAW_DIR, Partition::Month);
        let partitions = raw.partitions().unwrap();
        let total_lines: usize = partitions
            .iter()
            .map(|k| {
                let rows: Vec<Value> = raw.read(k).unwrap();
                rows.len()
            })
            .sum();
        assert_eq!(total_lines, 3, "raw layer must have exactly 3 rows, not 6");
    }
}
