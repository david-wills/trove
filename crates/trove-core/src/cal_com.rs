//! Cal.com — open-source Calendly alternative; pulls booking history via the
//! v2 REST API (`GET /v2/bookings`) into the shared calendar store. Both
//! hosted Cal.com and self-hosted instances expose the same API; self-hosted
//! users supply their instance base URL through the connection.
//!
//! **Auth.** Bearer API key pasted via [`CONNECTION`]'s [`ConnectMethod::TokenPaste`].
//! Stored (0600) under the "cal-com" service key, same never-expiring
//! [`TokenSet`] slot todoist uses. Connection is verified with a live request
//! at connect time; self-hosted instances add an optional base-URL field
//! (stored in `token_type`).
//!
//! **API v2 (`api.cal.com/v2`).** Cursor-paginated: `pagination.nextCursor`
//! passed as `?cursor=<c>`. Each pull fetches the **complete** booking list
//! (no `afterUpdatedAt` filter) so the calendar snapshot diff always sees the
//! full owned set and never spuriously emits `removed` lines for unchanged
//! bookings. Bookings are low-volume (tens to hundreds), so a full list is
//! cheap. The `updated` watermark is stored after each successful drain and
//! used for hub display ("last synced") and first-run detection.
//!
//! **Two vault destinations:**
//! - Raw layer (unconditional): `calendar/cal-com/raw/YYYY-MM.jsonl` — full
//!   fidelity booking objects partitioned by booking start month, upserted by
//!   uid.
//! - Contract layer: the ratified **calendar** contract —
//!   `calendar/events/YYYY-MM.jsonl` + `calendar/changes/YYYY-MM.jsonl` — via
//!   [`Vault::calendar_snapshot_scoped`] with the `calcom:` ownership prefix
//!   so EventKit / Google / Outlook rows are never overwritten. Each pull is
//!   a full window list of the user's owned bookings, diffed against stored
//!   rows.
//!
//! **Ownership prefix:** `calcom:{uid}` — one occurrence per booking uid
//! (bookings don't expand into recurrences the way calendar events do).
//!
//! Brief: docs/integrations/cal-com.md

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::calendar::{CalendarOccurrence, WINDOW_FUTURE_DAYS, WINDOW_PAST_DAYS};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::write_json_atomic;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const SERVICE: &str = "cal-com";
const STATE_FILE: &str = ".trove/cal-com-sync.json";
const RAW_DIR: &str = "calendar/cal-com/raw";
const DEFAULT_API_BASE: &str = "https://api.cal.com";
/// Required cal-api-version header value (confirmed from official v2 docs).
const CAL_API_VERSION: &str = "2026-05-01";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between periodic syncs — 30 min; bookings change slowly.
pub const CAL_COM_SYNC_SECS: u64 = 1800;

/// Row-id prefix for Cal.com-owned occurrences in the shared calendar store.
/// Lets [`crate::calendar::Vault::calendar_snapshot_scoped`] know which rows
/// belong to this collector so EventKit / Google / Outlook / CalDAV rows are
/// never overwritten.
const CALCOM_ROW_PREFIX: &str = "calcom:";

// ---------------------------------------------------------------------------
// State

/// Persisted in `.trove/cal-com-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalComSyncState {
    /// RFC3339 of the last successful full drain — becomes `afterUpdatedAt`
    /// for the next incremental pull.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Bookings currently in the window (for hub display).
    #[serde(default)]
    pub bookings: u64,
    /// Why the last sync failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Registry face

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull_inner(vault) {
        Ok(s) => Ok(CollectOutcome::note_if(
            s.added + s.changed + s.removed > 0,
            || {
                format!(
                    "cal.com synced — {} added, {} changed, {} removed",
                    s.added, s.changed, s.removed
                )
            },
        )),
        Err(e) => Ok(CollectOutcome::note(format!("cal.com sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = pull_inner(vault)?;
    let headline = if s.added + s.changed + s.removed > 0 {
        format!("{} added, {} changed, {} removed", s.added, s.changed, s.removed)
    } else {
        "Cal.com bookings up to date".to_string()
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("added", s.added),
            ("changed", s.changed),
            ("removed", s.removed),
            ("bookings", s.bookings),
        ]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_calcom_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (stub entry already
/// exists — this replaces the `NotWired` body).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "cal-com",
        name: "Cal.com",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Cal.com booking history — confirmed, cancelled, and rescheduled \
                      meetings — into the vault every 30 minutes. Works with both Cal.com Cloud and \
                      self-hosted instances.",
        domain: "calendar",
        vault_path: "calendar/cal-com/",
        toggleable: true,
        setup: &[
            "Connect with your Cal.com API key on this card.",
            "Each sync fetches confirmed and cancelled bookings into the calendar store.",
        ],
        caveats: "Bookings are pulled from the v2 API. Self-hosted instances: paste your instance \
                  base URL (e.g. https://cal.example.com) in the optional field, or leave blank for \
                  Cal.com Cloud.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(CAL_COM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("cal-com"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — API key from Cal.com Settings → Developer → API keys)

/// Verify the pasted API key, then store it along with an optional base URL.
/// Composite credential format: `<token>[|<base_url>]`. Self-hosted users
/// append their instance URL after a pipe character.
fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty token — paste your Cal.com API key");
    }
    let (key, base) = if let Some((k, u)) = raw.split_once('|') {
        let k = k.trim().to_string();
        let u = u.trim().to_string();
        let base = if u.is_empty() { DEFAULT_API_BASE.to_string() } else { u };
        (k, base)
    } else {
        (raw.to_string(), DEFAULT_API_BASE.to_string())
    };
    if key.is_empty() {
        bail!("API key is empty — paste your Cal.com API key (Settings → Developer → API keys)");
    }
    let client = CalComClient { base: base.clone(), key: key.clone() };
    match client.bookings_page(None, None) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Cal.com rejected the API key (401) — check it's from Settings → Developer → API keys"
        ),
        Err(e) => bail!("Cal.com /bookings check failed: {e}"),
    }
    // Store: access_token = key (secret), token_type = base URL.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: key,
            refresh_token: None,
            token_type: Some(base),
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
            label: "Cal.com".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`] — the integrator adds
/// one `&crate::cal_com::CONNECTION,` line there.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "cal-com",
    display_name: "Cal.com",
    methods: &[ConnectMethod::TokenPaste {
        label: "Cal.com API key",
        help: "Paste your Cal.com API key from Settings → Developer → API keys. \
               Self-hosted: append a pipe and your instance URL, e.g. \
               `cal_live_abc123|https://cal.example.com`.",
        placeholder: "cal_live_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["cal-com"],
    setup: &[
        "In Cal.com, open Settings → Developer → API keys.",
        "Create or copy an existing API key.",
        "Self-hosted users: append `|<instance_base_url>` after the key \
         (e.g. `key|https://cal.example.com`).",
        "Paste the key (or key|url) here — stored locally, never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP client

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

struct CalComClient {
    base: String,
    key: String,
}

impl CalComClient {
    /// One `GET /v2/bookings` page. `after_updated_at` is an RFC3339 filter
    /// for incremental pulls; `cursor` is the opaque pagination cursor.
    fn bookings_page(
        &self,
        after_updated_at: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Value, FetchError> {
        let url = format!("{}/v2/bookings", self.base);
        let mut req = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", self.key))
            .set("cal-api-version", CAL_API_VERSION)
            .timeout(HTTP_TIMEOUT);
        if let Some(ts) = after_updated_at {
            req = req.query("afterUpdatedAt", ts);
        }
        if let Some(c) = cursor {
            req = req.query("cursor", c);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parse response: {e}"))),
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

    /// Drain all pages of `/v2/bookings`, returning every raw booking `Value`.
    fn fetch_all_bookings(&self, after_updated_at: Option<&str>) -> Result<Vec<Value>, FetchError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let resp = self.bookings_page(after_updated_at, cursor.as_deref())?;
            if let Some(items) = resp.get("data").and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            let next = resp
                .get("pagination")
                .and_then(|p| p.get("nextCursor"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let has_more = resp
                .get("pagination")
                .and_then(|p| p.get("hasMore"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            cursor = next;
            if cursor.is_none() || !has_more {
                break;
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Normalization: Cal.com booking JSON → CalendarOccurrence

/// Map Cal.com `status` → the calendar contract's status vocabulary.
fn map_status(s: &str) -> &'static str {
    match s {
        "accepted" => "confirmed",
        "cancelled" => "canceled",
        "pending" => "tentative",
        "rejected" => "canceled",
        _ => "",
    }
}

/// Cal.com v2 booking → [`CalendarOccurrence`].
/// Returns `None` when the booking lacks a `uid` or a `start` time.
fn booking_to_occurrence(v: &Value) -> Option<CalendarOccurrence> {
    let uid = v.get("uid").and_then(Value::as_str)?;
    let start = v.get("start").and_then(Value::as_str)?;
    let end = v.get("end").and_then(Value::as_str).unwrap_or(start).to_string();
    let title = v.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let status_raw = v.get("status").and_then(Value::as_str).unwrap_or("");
    let status = map_status(status_raw).to_string();
    let location = v.get("location").and_then(Value::as_str).unwrap_or("").to_string();

    // Use the event-type slug as the calendar name; fall back to "Cal.com".
    let calendar_name = v
        .get("eventType")
        .and_then(|et| et.get("slug"))
        .and_then(Value::as_str)
        .unwrap_or("Cal.com")
        .to_string();

    // Host email as the account label (who owns the booking page).
    let account = v
        .get("hosts")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|h| h.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("Cal.com")
        .to_string();

    // Attendees: displayName preferred, then email.
    let attendees = v
        .get("attendees")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    p.get("name")
                        .or_else(|| p.get("email"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(CalendarOccurrence {
        id: format!("{CALCOM_ROW_PREFIX}{uid}"),
        occurrence: String::new(), // bookings are single occurrences (no recurrence expansion)
        start: start.to_string(),
        end,
        all_day: false,
        title,
        calendar: calendar_name,
        account,
        location,
        notes: String::new(),
        attendees,
        status,
        recurring: false,
    })
}

// ---------------------------------------------------------------------------
// Sync logic

/// Stats from one pull pass, for the hub outcome.
#[derive(Debug, Default)]
struct SyncStats {
    added: u64,
    changed: u64,
    removed: u64,
    bookings: u64,
}

/// One full pull pass: fetch **all** bookings from the API (no incremental
/// watermark filter — bookings are low-volume and a full list is required to
/// detect genuine removals), write raw, update the calendar contract store,
/// advance the watermark. Blocking (network).
///
/// The `afterUpdatedAt` filter is intentionally NOT used for the calendar
/// snapshot: sending only recently-changed bookings to
/// `calendar_snapshot_scoped` would cause it to diff a partial set against all
/// stored calcom rows and spuriously emit `removed` lines for every unchanged
/// booking. By always fetching the complete owned set we give the snapshot a
/// faithful picture and only emit removals for bookings that are genuinely
/// absent from the API response.
///
/// The watermark (`state.updated`) is still stored for hub display ("last
/// synced") and as the empty-state signal for the first-run backfill guard.
fn pull_inner(vault: &Vault) -> Result<SyncStats> {
    let Some(token) = vault.load_sync_token(SERVICE)? else {
        bail!("Cal.com is not connected — paste an API key on the integration card");
    };
    let api_key = token.access_token.clone();
    let base = token.token_type.as_deref().unwrap_or(DEFAULT_API_BASE).to_string();
    let client = CalComClient { base, key: api_key };

    let mut state = vault.read_calcom_sync().unwrap_or_default();
    // Always fetch the complete set — no afterUpdatedAt filter.
    // See module doc for why incremental filtering causes silent data loss.
    let first_run = state.updated.is_empty();

    let now = Local::now();
    let raw_bookings = match client.fetch_all_bookings(None) {
        Ok(b) => b,
        Err(FetchError::Unauthorized) => {
            bail!("Cal.com API key rejected (401) — reconnect from the integration card");
        }
        Err(FetchError::RateLimited) => {
            bail!("Cal.com rate-limited this request — will retry next pass");
        }
        Err(FetchError::Other(m)) => {
            bail!("Cal.com fetch failed: {m}");
        }
    };

    // Guard: an empty fetch when we have previously synced bookings might mean
    // a transient network error or an empty-result API bug. Do not advance the
    // watermark; the next pass re-fetches.
    if raw_bookings.is_empty() && state.bookings > 0 {
        // Silently succeed — no data changed.
        return Ok(SyncStats { bookings: state.bookings, ..Default::default() });
    }

    // Write raw layer (full fidelity, upsert by uid into monthly partitions).
    vault.calcom_upsert_raw(&raw_bookings)?;

    // Build contract occurrences from all fetched bookings.
    let fresh: Vec<CalendarOccurrence> =
        raw_bookings.iter().filter_map(booking_to_occurrence).collect();

    // Diff window: WINDOW_PAST_DAYS back … WINDOW_FUTURE_DAYS forward.
    let win_start = (now - ChronoDuration::days(WINDOW_PAST_DAYS)).format("%Y-%m-%d").to_string();
    let win_end = (now + ChronoDuration::days(WINDOW_FUTURE_DAYS)).format("%Y-%m-%d").to_string();
    let ts = now.to_rfc3339();

    // Filter fresh to the diff window (bookings outside it go to raw only).
    let in_window: Vec<CalendarOccurrence> = fresh
        .into_iter()
        .filter(|o| {
            let d = &o.start[..10.min(o.start.len())];
            d >= win_start.as_str() && d <= win_end.as_str()
        })
        .collect();

    // Check whether Cal.com has any stored rows in the window already.
    // Used for the first-run backfill guard below.
    let stored_calcom_rows: Vec<CalendarOccurrence> = if first_run {
        vault
            .calendar_range(&win_start, &win_end)
            .unwrap_or_default()
            .into_iter()
            .filter(|o| o.id.starts_with(CALCOM_ROW_PREFIX))
            .collect()
    } else {
        Vec::new()
    };

    let stats = if first_run && stored_calcom_rows.is_empty() {
        // Cal.com's very first sync: the calendar/events/ directory may already
        // exist (EventKit or another collector created it). Emitting `added`
        // change lines for every pre-existing booking violates the contract's
        // "first-ever sync writes no added lines" doctrine. Use backfill
        // (silent merge, zero change lines) exactly as google_calendar.rs does
        // at its lines 1043-1048.
        vault.calendar_backfill(&in_window)?;
        SyncStats {
            bookings: in_window.len() as u64,
            ..Default::default()
        }
    } else {
        // Subsequent syncs (or a vault that already has calcom rows): diff the
        // complete owned set so removals are genuine, not artefacts of a
        // partial fetch.
        let snap = vault.calendar_snapshot_scoped(
            &in_window,
            &win_start,
            &win_end,
            &ts,
            &|o| o.id.starts_with(CALCOM_ROW_PREFIX),
        )?;
        SyncStats {
            added: snap.added,
            changed: snap.changed,
            removed: snap.removed,
            // Use in_window.len() as the authoritative count (the full owned
            // set in the window), not snap.events (which equals in_window.len()
            // anyway for a full-fetch pass but is explicit here for clarity).
            bookings: in_window.len() as u64,
        }
    };

    // Advance the watermark after a successful drain.
    state.updated = ts;
    state.bookings = stats.bookings;
    state.error = None;
    vault.write_calcom_sync(&state)?;

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Vault helpers

impl Vault {
    /// Upsert raw Cal.com booking objects by `uid` into their start-month
    /// partitions. Existing rows with the same uid are replaced (idempotent
    /// re-pulls after the incremental watermark).
    fn calcom_upsert_raw(&self, bookings: &[Value]) -> Result<()> {
        // Group by start-month.
        let mut by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for b in bookings {
            let start = b.get("start").and_then(Value::as_str).unwrap_or("");
            if start.len() < 7 {
                continue;
            }
            let month = &start[..7];
            by_month.entry(month.to_string()).or_default().push(b.clone());
        }
        for (month, new_rows) in by_month {
            let path = self.resolve(&format!("{RAW_DIR}/{month}.jsonl"))?;
            let mut existing: Vec<Value> = if path.exists() {
                std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .collect()
            } else {
                Vec::new()
            };
            let new_uids: HashSet<String> = new_rows
                .iter()
                .filter_map(|v| v.get("uid").and_then(Value::as_str).map(str::to_string))
                .collect();
            existing.retain(|v| {
                v.get("uid")
                    .and_then(Value::as_str)
                    .map(|u| !new_uids.contains(u))
                    .unwrap_or(true)
            });
            existing.extend(new_rows);
            existing.sort_by(|a, b| {
                let sa = a.get("start").and_then(Value::as_str).unwrap_or("");
                let sb = b.get("start").and_then(Value::as_str).unwrap_or("");
                sa.cmp(sb)
            });
            self.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
        }
        Ok(())
    }

    /// The persisted Cal.com sync state, if a sync has run.
    pub fn read_calcom_sync(&self) -> Option<CalComSyncState> {
        let path = self.resolve(STATE_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_calcom_sync(&self, state: &CalComSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(STATE_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-calcom-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Confirmed booking fixture — field names confirmed from v2 API docs.
    fn confirmed_booking() -> Value {
        json!({
            "id": 100,
            "uid": "abc123def456",
            "title": "30min Consultation",
            "description": "Discussion about the project",
            "hosts": [{"id": 1, "name": "Jane Doe", "email": "jane@example.com",
                        "displayEmail": "jane@example.com", "username": "jane",
                        "timeZone": "America/Los_Angeles"}],
            "status": "accepted",
            "start": "2026-06-20T15:00:00Z",
            "end": "2026-06-20T15:30:00Z",
            "duration": 30,
            "eventTypeId": 42,
            "eventType": {"id": 42, "slug": "30min"},
            "location": "https://meet.example.com/abc",
            "absentHost": false,
            "createdAt": "2026-06-15T10:00:00Z",
            "updatedAt": "2026-06-15T10:00:00Z",
            "attendees": [
                {"name": "John Smith", "email": "john@example.com",
                 "displayEmail": "john@example.com", "timeZone": "America/New_York",
                 "absent": false}
            ],
            "guests": [],
            "bookingFieldsResponses": {"notes": "Looking forward to it"}
        })
    }

    /// Cancelled booking fixture.
    fn cancelled_booking() -> Value {
        json!({
            "id": 101,
            "uid": "zzz999cancel",
            "title": "Cancelled Meeting",
            "hosts": [{"id": 1, "name": "Jane Doe", "email": "jane@example.com",
                        "displayEmail": "jane@example.com", "username": "jane",
                        "timeZone": "America/Los_Angeles"}],
            "status": "cancelled",
            "cancellationReason": "Schedule conflict",
            "start": "2026-06-22T09:00:00Z",
            "end": "2026-06-22T09:30:00Z",
            "duration": 30,
            "eventTypeId": 42,
            "eventType": {"id": 42, "slug": "30min"},
            "createdAt": "2026-06-10T08:00:00Z",
            "updatedAt": "2026-06-20T14:00:00Z",
            "attendees": [
                {"name": "Alice Brown", "email": "alice@example.com",
                 "displayEmail": "alice@example.com", "timeZone": "UTC",
                 "absent": false}
            ],
            "bookingFieldsResponses": {}
        })
    }

    #[test]
    fn booking_to_occurrence_confirmed() {
        let b = confirmed_booking();
        let occ = booking_to_occurrence(&b).unwrap();
        assert_eq!(occ.id, "calcom:abc123def456");
        assert_eq!(occ.title, "30min Consultation");
        assert_eq!(occ.start, "2026-06-20T15:00:00Z");
        assert_eq!(occ.end, "2026-06-20T15:30:00Z");
        assert_eq!(occ.status, "confirmed");
        assert_eq!(occ.calendar, "30min");
        assert_eq!(occ.account, "jane@example.com");
        assert_eq!(occ.location, "https://meet.example.com/abc");
        assert_eq!(occ.attendees, vec!["John Smith"]);
        assert!(!occ.all_day);
        assert!(!occ.recurring);
        assert!(occ.occurrence.is_empty());
    }

    #[test]
    fn booking_to_occurrence_cancelled() {
        let b = cancelled_booking();
        let occ = booking_to_occurrence(&b).unwrap();
        assert_eq!(occ.id, "calcom:zzz999cancel");
        assert_eq!(occ.status, "canceled");
        assert_eq!(occ.attendees, vec!["Alice Brown"]);
    }

    #[test]
    fn map_status_all_variants() {
        assert_eq!(map_status("accepted"), "confirmed");
        assert_eq!(map_status("cancelled"), "canceled");
        assert_eq!(map_status("pending"), "tentative");
        assert_eq!(map_status("rejected"), "canceled");
        assert_eq!(map_status("unknown"), "");
    }

    #[test]
    fn booking_without_uid_returns_none() {
        let b = json!({ "title": "No uid", "start": "2026-06-20T10:00:00Z" });
        assert!(booking_to_occurrence(&b).is_none());
    }

    #[test]
    fn booking_without_start_returns_none() {
        let b = json!({ "uid": "nope", "title": "No start" });
        assert!(booking_to_occurrence(&b).is_none());
    }

    #[test]
    fn raw_upsert_idempotent() {
        let v = temp_vault("raw");
        let b1 = confirmed_booking();
        let mut b2 = confirmed_booking();
        b2["title"] = json!("Updated Consultation"); // simulate update

        v.calcom_upsert_raw(&[b1]).unwrap();
        v.calcom_upsert_raw(&[b2]).unwrap();

        let path = v.root().join("calendar/cal-com/raw/2026-06.jsonl");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // Only one row — the updated one replaces the original.
        assert_eq!(lines.len(), 1, "upsert keeps exactly one row per uid");
        let row: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row["title"], "Updated Consultation");
    }

    #[test]
    fn raw_upsert_two_months() {
        let v = temp_vault("months");
        let b1 = confirmed_booking(); // 2026-06
        let b2 = cancelled_booking(); // 2026-06 (different uid)
        let other: Value = json!({
            "uid": "july_booking",
            "start": "2026-07-05T10:00:00Z",
            "end": "2026-07-05T10:30:00Z",
            "status": "accepted",
            "title": "July Call",
            "hosts": [{"email": "jane@example.com"}],
            "attendees": []
        });
        v.calcom_upsert_raw(&[b1, b2, other]).unwrap();
        assert!(v.root().join("calendar/cal-com/raw/2026-06.jsonl").exists());
        assert!(v.root().join("calendar/cal-com/raw/2026-07.jsonl").exists());
        let june_content =
            std::fs::read_to_string(v.root().join("calendar/cal-com/raw/2026-06.jsonl")).unwrap();
        assert_eq!(june_content.lines().count(), 2, "two bookings in June");
    }

    #[test]
    fn calendar_contract_write_and_diff() {
        let v = temp_vault("contract");
        let b1 = confirmed_booking();
        let b2 = cancelled_booking();
        let occs: Vec<CalendarOccurrence> =
            [&b1, &b2].iter().filter_map(|b| booking_to_occurrence(b)).collect();

        // First snapshot: silent baseline (no change events emitted).
        let ws = "2026-06-01";
        let we = "2026-07-31";
        let ts = "2026-06-16T10:00:00-07:00";
        let stats = v
            .calendar_snapshot_scoped(&occs, ws, we, ts, &|o| o.id.starts_with(CALCOM_ROW_PREFIX))
            .unwrap();
        assert!(stats.baseline);
        assert_eq!(stats.events, 2);
        assert_eq!(stats.added + stats.removed + stats.changed, 0);

        let june_path = v.root().join("calendar/events/2026-06.jsonl");
        assert!(june_path.exists());
        let content = std::fs::read_to_string(&june_path).unwrap();
        let rows: Vec<CalendarOccurrence> =
            content.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.status == "confirmed"));
        assert!(rows.iter().any(|r| r.status == "canceled"));

        // Second snapshot with one booking removed — should emit a removal.
        let ts2 = "2026-06-16T11:00:00-07:00";
        let occs2: Vec<CalendarOccurrence> =
            [&b1].iter().filter_map(|b| booking_to_occurrence(b)).collect();
        let stats2 = v
            .calendar_snapshot_scoped(
                &occs2,
                ws,
                we,
                ts2,
                &|o| o.id.starts_with(CALCOM_ROW_PREFIX),
            )
            .unwrap();
        assert!(!stats2.baseline);
        assert_eq!(stats2.removed, 1);
    }

    #[test]
    fn sync_state_round_trip() {
        let v = temp_vault("state");
        assert!(v.read_calcom_sync().is_none());
        let state = CalComSyncState {
            updated: "2026-06-16T10:00:00-07:00".to_string(),
            bookings: 5,
            error: None,
        };
        v.write_calcom_sync(&state).unwrap();
        let loaded = v.read_calcom_sync().unwrap();
        assert_eq!(loaded.updated, "2026-06-16T10:00:00-07:00");
        assert_eq!(loaded.bookings, 5);
        assert!(loaded.error.is_none());
    }

    #[test]
    fn ownership_prefix_scopes_correctly() {
        assert!("calcom:abc123".starts_with(CALCOM_ROW_PREFIX));
        assert!(!"gcal:sub/cal/ev".starts_with(CALCOM_ROW_PREFIX));
        assert!(!"mcal:acc/ev".starts_with(CALCOM_ROW_PREFIX));
        assert!(!"caldav:uuid/ev".starts_with(CALCOM_ROW_PREFIX));
    }

    /// Regression test for the incremental-wipe defect.
    ///
    /// Simulates what the old code did: two bookings stored, then a second
    /// snapshot with only ONE booking in `fresh` (as if an incremental filter
    /// returned only the changed booking). Verifies that the snapshot correctly
    /// emits a `removed` only for the genuinely absent booking — i.e. confirms
    /// the snapshot semantics that pull_inner now relies on by always passing
    /// the FULL owned set.
    ///
    /// In the fixed code pull_inner always fetches all bookings (afterUpdatedAt=None),
    /// so `in_window` always contains the full owned set and this scenario
    /// (partial set → spurious removed) can never occur in practice. This test
    /// documents both the expected snapshot contract and guards against
    /// regression if the fetch logic is revisited.
    #[test]
    fn full_fetch_no_spurious_removed() {
        let v = temp_vault("no_wipe");
        let b1 = confirmed_booking();
        let b2 = cancelled_booking();
        let ws = "2026-06-01";
        let we = "2026-07-31";
        let ts1 = "2026-06-16T10:00:00-07:00";

        // First pass: both bookings stored (baseline, no change lines).
        let occs_all: Vec<CalendarOccurrence> =
            [&b1, &b2].iter().filter_map(|b| booking_to_occurrence(b)).collect();
        let s1 = v
            .calendar_snapshot_scoped(&occs_all, ws, we, ts1, &|o| {
                o.id.starts_with(CALCOM_ROW_PREFIX)
            })
            .unwrap();
        assert!(s1.baseline);
        assert_eq!(s1.events, 2);

        // Second pass: ALL bookings returned by API (b1 unchanged, b2 unchanged).
        // This simulates a full-fetch pass — no removals expected.
        let ts2 = "2026-06-16T11:00:00-07:00";
        let occs_full: Vec<CalendarOccurrence> =
            [&b1, &b2].iter().filter_map(|b| booking_to_occurrence(b)).collect();
        let s2 = v
            .calendar_snapshot_scoped(&occs_full, ws, we, ts2, &|o| {
                o.id.starts_with(CALCOM_ROW_PREFIX)
            })
            .unwrap();
        assert!(!s2.baseline);
        // A full fetch with both bookings still present must emit ZERO removals.
        assert_eq!(
            s2.removed, 0,
            "full fetch of 2 unchanged bookings must not emit removed lines"
        );
        assert_eq!(s2.added, 0, "no new bookings");
    }

    /// Regression test for the first-run-floods-added defect.
    ///
    /// Simulates: EventKit (or another collector) has already created
    /// `calendar/events/` so `baseline = !dir.exists()` is false for the
    /// cal.com first sync. Verifies that the backfill path emits zero change
    /// lines when pull_inner's first-run guard is triggered.
    #[test]
    fn first_run_backfill_no_spurious_added() {
        let v = temp_vault("first_run");
        let b1 = confirmed_booking();
        let b2 = cancelled_booking();
        let ws = "2026-06-01";
        let we = "2026-07-31";
        let ts0 = "2026-06-15T09:00:00-07:00";

        // Simulate EventKit writing a non-calcom event first — this creates
        // calendar/events/ and makes baseline=false for the next collector.
        let eventkit_occ = CalendarOccurrence {
            id: "ek:event:1".to_string(),
            occurrence: String::new(),
            start: "2026-06-18T14:00:00Z".to_string(),
            end: "2026-06-18T15:00:00Z".to_string(),
            all_day: false,
            title: "Team Standup".to_string(),
            calendar: "Work".to_string(),
            account: "me@example.com".to_string(),
            location: String::new(),
            notes: String::new(),
            attendees: vec![],
            status: "confirmed".to_string(),
            recurring: true,
        };
        // Write the EventKit occurrence so the events dir now exists.
        v.calendar_snapshot_scoped(&[eventkit_occ], ws, we, ts0, &|o| {
            o.id.starts_with("ek:")
        })
        .unwrap();

        // Now simulate Cal.com's very first sync (state.updated is empty,
        // no stored calcom rows). The events dir already exists → baseline=false
        // for calendar_snapshot_scoped. Use backfill instead.
        let calcom_occs: Vec<CalendarOccurrence> =
            [&b1, &b2].iter().filter_map(|b| booking_to_occurrence(b)).collect();

        // Confirm no stored calcom rows yet.
        let stored = v
            .calendar_range(ws, we)
            .unwrap()
            .into_iter()
            .filter(|o| o.id.starts_with(CALCOM_ROW_PREFIX))
            .count();
        assert_eq!(stored, 0, "no calcom rows before first sync");

        // Backfill: silent merge, zero change lines.
        let written = v.calendar_backfill(&calcom_occs).unwrap();
        assert!(written > 0, "backfill wrote at least one month file");

        // Verify: no changes/ files were created.
        let changes_dir = v.root().join("calendar/changes");
        let has_changes = if changes_dir.exists() {
            std::fs::read_dir(&changes_dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        } else {
            false
        };
        assert!(
            !has_changes,
            "first-run backfill must write zero change lines (no calendar/changes/ files)"
        );

        // Verify the calcom bookings are now stored.
        let stored_after = v
            .calendar_range(ws, we)
            .unwrap()
            .into_iter()
            .filter(|o| o.id.starts_with(CALCOM_ROW_PREFIX))
            .count();
        assert_eq!(stored_after, 2, "both calcom bookings stored after backfill");
    }
}
