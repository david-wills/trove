//! Google Calendar collector — events from every connected Google account
//! into the same calendar stores as EventKit (`calendar/events/` +
//! `calendar/changes/`), deduped against the macOS Calendar source, kept
//! current by Calendar API sync tokens. The OAuth side (connect, token
//! store, refresh, reconnect flagging) is owned by [`crate::sync::google`];
//! this module only asks it for a fresh token per account via
//! [`crate::sync::google::fresh_token`]. See [`crate::gmail`] for the worked
//! per-account-pull template this follows.
//!
//! **Target.** Occurrences land in the unified calendar snapshot
//! ([`crate::calendar`]) with ids of the form
//! `gcal:{sub}/{calendarId}/{eventId}` — the `gcal:` prefix
//! ([`crate::calendar::GOOGLE_ROW_PREFIX`]) is the ownership marker that
//! lets the two collectors share one store: each snapshot pass diffs and
//! rewrites only its own rows ([`Vault::calendar_snapshot_scoped`]), so an
//! EventKit pass can never mass-"remove" Google-pulled rows or vice versa.
//! `eventId` is the *series* id (Google's `recurringEventId` when set), so
//! all instances of a recurring event share one row id and the store's
//! `id + occurrence` diff key works unchanged. Reschedules and cancellations
//! flow through the same [`crate::calendar::diff_occurrences`] machinery
//! into `calendar/changes/`.
//!
//! **The EventKit dedupe** (the make-or-break detail). A Google account
//! added to macOS already arrives via EventKit, and those events must not
//! double. What's actually on disk for EventKit rows is
//! `EKEvent.eventIdentifier`, which on macOS is
//! `{localStoreUUID}:{calendarItemExternalIdentifier}[/RID=...]` — and the
//! external identifier is the event's **iCalUID**, which for Google events
//! is `{eventId}@google.com` (verified empirically against a real vault:
//! `AB2C4FD5-…:2cg115mfr3nr2634ok2nqiodl9_R20260302T180000@google.com/RID=…`
//! for a Google Workspace event). The Google API exposes `iCalUID` directly
//! on every event, so the dedupe key is: *an event is EventKit-covered when
//! its `iCalUID` equals the substring of any EventKit-owned row's id between
//! the first `:` and the `/RID=` suffix*. The raw Google `id` would *not*
//! match what's on disk (the eventIdentifier's leading half is a local
//! UUID), which is why iCalUID is the key. The same mechanism dedupes a
//! calendar shared between two connected Google accounts: the
//! earlier-sorted account owns the rows, later accounts defer by series id.
//!
//! Covered events are tracked per calendar in the state file (series id →
//! last in-window day, plus the rare iCalUID that doesn't follow the
//! `{id}@google.com` convention, e.g. events imported into Google from
//! elsewhere). When a covered event's EventKit row disappears from the
//! window (the account was removed from macOS), coverage is "lost" and the
//! calendar resets to a full window re-list so the events reappear as
//! Google-pulled rows; when a stored Google row's iCalUID shows up in
//! EventKit (the account was just added to macOS), the row is handed off —
//! it drops from the Google slice (the diff logs it `removed`; EventKit's
//! own pass logs its copy `added` — source-handoff noise, not data loss).
//!
//! **Two list modes per calendar, resumable:**
//!
//! 1. **Full window list** — `events.list` with `timeMin`/`timeMax` spanning
//!    the store's sync window ([`crate::calendar::WINDOW_PAST_DAYS`] /
//!    [`WINDOW_FUTURE_DAYS`]) and `singleEvents=true` so recurrences arrive
//!    expanded to instances, matching EventKit's shape. Runs on first sync
//!    (merged silently via [`Vault::calendar_backfill`] — years of existing
//!    events are state, not "added" events), after a sync-token expiry
//!    (HTTP 410), after a coverage transition, and every
//!    [`FULL_REFRESH_DAYS`] so the rolling window's far edge and any missed
//!    transition self-heal.
//! 2. **Incremental** — the `nextSyncToken` from the previous list yields
//!    only changed/cancelled instances; a no-change pass is one cheap
//!    request per calendar, which is what affords the 15-minute cadence.
//!
//! State lives in `.trove/google-calendar-sync.json` (per account, per
//! calendar: sync token, last-full-list day, covered map, uid exceptions),
//! written atomically after every calendar so an interrupted pass resumes
//! at the next calendar with nothing advanced past what was stored. A
//! silent no-op when no Google account is connected; per-account failures
//! are recorded in the state file and never abort other accounts. A
//! human-readable status table is kept at `calendar/google.md`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{Duration as ChronoDuration, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::calendar::{
    CalendarOccurrence, GOOGLE_ROW_PREFIX, WINDOW_FUTURE_DAYS, WINDOW_PAST_DAYS,
};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::sync::google::GoogleAccountInfo;
use crate::vault::Vault;

/// Seconds between Google Calendar passes in the watcher loop. Matches
/// Gmail's freshness; sync tokens make a no-change pass one cheap request
/// per calendar.
pub const GCAL_SYNC_SECS: u64 = 900;

// A no-change pass is one sync-token request per calendar; a silent no-op
// when no Google account is connected.
fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_google_calendar()?;
    Ok(crate::registry::CollectOutcome::note_if(
        s.baselined + s.added + s.changed + s.removed > 0,
        || {
            if s.baselined > 0 {
                format!(
                    "google calendar baseline — {} events across {} calendars",
                    s.baselined, s.calendars
                )
            } else {
                format!(
                    "google calendar synced — {} added, {} changed, {} removed",
                    s.added, s.changed, s.removed
                )
            }
        },
    ))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_gcal_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-calendar",
        name: "Google Calendar",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Events from every connected Google account into the same calendar store as EventKit, deduped by Google event id, with a reschedule/cancellation change stream kept by sync tokens.",
        domain: "calendar",
        vault_path: "calendar/",
        toggleable: true,
        setup: &[],
        caveats: "Deduped against the macOS Calendar source, so events already arriving via EventKit aren't doubled. Pulling directly means changes flow even when the account isn't added to macOS.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(GCAL_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google"),
    pull: Some(pull),
};

/// [`crate::registry::IntegrationDef::pull`] adapter:
/// [`Vault::google_calendar_pull`] mapped into the generic outcome shape.
/// Per-account failures never abort the pass — they land in
/// `.trove/google-calendar-sync.json` — so the headline re-reads the state
/// to surface them rather than reporting a clean sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.google_calendar_pull()?;
    let errors = vault
        .read_gcal_sync()
        .map(|st| st.accounts.values().filter(|a| a.error.is_some()).count() as u64)
        .unwrap_or(0);
    let mut headline = if s.baselined > 0 {
        format!(
            "{} events baselined across {} calendars",
            s.baselined, s.calendars
        )
    } else if s.added + s.changed + s.removed > 0 {
        format!(
            "{} added, {} changed, {} removed",
            s.added, s.changed, s.removed
        )
    } else {
        "calendars up to date".to_string()
    };
    if errors > 0 {
        headline.push_str(&format!(
            " — {errors} account{} failed",
            if errors == 1 { "" } else { "s" }
        ));
    }
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("accounts", s.accounts as u64),
            ("calendars", s.calendars as u64),
            ("baselined", s.baselined),
            ("added", s.added),
            ("changed", s.changed),
            ("removed", s.removed),
            ("account_errors", errors),
        ]),
    })
}

const STATE_FILE: &str = ".trove/google-calendar-sync.json";
const INDEX_FILE: &str = "calendar/google.md";
const GCAL_API: &str = "https://www.googleapis.com";
/// Kept short so a hung connection can't stall the watcher owner loop for
/// long (the `oura.rs` reasoning).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// `events.list` / `calendarList.list` page size (the API maximum).
const PAGE_SIZE: u32 = 250;
/// Days between forced full window re-lists per calendar. Sync tokens are
/// pinned to the window they were minted against, so a periodic full list
/// rolls the window forward, refreshes the covered map's days, and heals
/// any coverage transition the per-pass checks missed.
const FULL_REFRESH_DAYS: i64 = 30;

/// Result of one Google Calendar sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GcalSyncStats {
    /// Accounts whose pass completed without a status-level error.
    pub accounts: u32,
    /// Calendars synced this pass.
    pub calendars: u32,
    /// Occurrences written silently by first-sync calendar baselines.
    pub baselined: u64,
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
}

/// Per-calendar sync progress, nested in the state file. Deleting a
/// calendar's entry re-runs its (silent-merge-free) full list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GcalCalendarState {
    /// Calendar display name, for the index.
    #[serde(default)]
    pub summary: String,
    /// The incremental cursor from the last completed list; `None` forces a
    /// full window re-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_token: Option<String>,
    /// Day (YYYY-MM-DD) of the last full window list — drives the
    /// [`FULL_REFRESH_DAYS`] self-heal.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub full_synced: String,
    /// Events currently suppressed as EventKit-covered: series id → the
    /// latest in-window start day seen, which is what lets stale entries be
    /// pruned once they scroll out of the window.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub covered: BTreeMap<String, String>,
    /// Series whose iCalUID does *not* follow the `{id}@google.com`
    /// convention (events imported into Google from elsewhere) — needed to
    /// re-derive the dedupe key for rows already on disk.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub uid_exceptions: BTreeMap<String, String>,
    /// Occurrences currently written for this calendar (drives the index).
    #[serde(default)]
    pub events: u64,
}

/// Per-account sync progress (keyed by Google `sub` in the state file).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GcalAccountState {
    /// Display address (for the index / UI; the map key is the `sub`).
    #[serde(default)]
    pub email: String,
    /// Per-calendar progress, keyed by Google calendar id.
    #[serde(default)]
    pub calendars: BTreeMap<String, GcalCalendarState>,
    /// Why this account's last pass failed, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The whole Google Calendar sync state, persisted at
/// `.trove/google-calendar-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GcalSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Per-account progress, keyed by Google `sub`.
    pub accounts: BTreeMap<String, GcalAccountState>,
}

/// Status-level fetch errors needing distinct handling.
#[derive(Debug)]
enum FetchError {
    /// 429, or Calendar's 403 `rateLimitExceeded` family.
    RateLimited,
    Unauthorized,
    /// HTTP 410 GONE: the sync token expired — reset to a full re-list.
    SyncGone,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429/403)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::SyncGone => write!(f, "sync token expired (HTTP 410)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// A vault write error inside the API-fetch path → a soft `FetchError`.
fn soft(e: anyhow::Error) -> FetchError {
    FetchError::Other(format!("{e:#}"))
}

/// Map a status-level error to a user-facing message for the state file.
fn status_error(account: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::RateLimited => {
            anyhow!("Google Calendar rate limited the sync ({account}) — it resumes next pass")
        }
        FetchError::Unauthorized => anyhow!(
            "Google Calendar rejected the token ({account}, 401) — reconnect from the Integrations tab"
        ),
        FetchError::SyncGone => {
            anyhow!("Google Calendar sync token expired ({account}) — re-listing")
        }
        FetchError::Other(m) => anyhow!("google calendar {account}: {m}"),
    }
}

// ---------------------------------------------------------------------------
// API client

/// One subscribed calendar from `calendarList.list`.
struct GcalCalendar {
    id: String,
    summary: String,
}

/// How to list one calendar's events.
enum EventsQuery {
    /// Windowed full list (`timeMin`/`timeMax`).
    Full { page_token: Option<String> },
    /// Changes since the stored sync token.
    Incremental { sync_token: String, page_token: Option<String> },
}

/// One `events.list` page: raw event JSON plus the paging/sync cursors
/// (`next_sync_token` arrives on the final page).
struct EventsPage {
    items: Vec<Value>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
}

/// Thin Calendar API client. The base URL is injected so the sync logic is
/// testable against scripted responses (the `oura.rs`/`gmail.rs` pattern).
struct GcalClient {
    base: String,
    token: String,
}

impl GcalClient {
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
            Err(ureq::Error::Status(410, _)) => Err(FetchError::SyncGone),
            // The Calendar API reports quota exhaustion as 403
            // (`rateLimitExceeded`) as often as 429 — treat both as
            // retry-next-pass.
            Err(ureq::Error::Status(403, _)) | Err(ureq::Error::Status(429, _)) => {
                Err(FetchError::RateLimited)
            }
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

    /// Every subscribed calendar (deleted entries skipped), all pages.
    fn list_calendars(&self) -> Result<Vec<GcalCalendar>, FetchError> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut params = vec![("maxResults", PAGE_SIZE.to_string())];
            if let Some(t) = &page_token {
                params.push(("pageToken", t.clone()));
            }
            let v = self.get("/calendar/v3/users/me/calendarList", &params)?;
            if let Some(items) = v.get("items").and_then(Value::as_array) {
                for it in items {
                    let Some(id) = it.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if it.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
                        continue;
                    }
                    // summaryOverride is the user's rename of a subscribed
                    // calendar — what they'd recognize.
                    let summary = it
                        .get("summaryOverride")
                        .or_else(|| it.get("summary"))
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string();
                    out.push(GcalCalendar { id: id.to_string(), summary });
                }
            }
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(t) => page_token = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    /// One `events.list` page. `singleEvents=true` always, so recurrences
    /// arrive expanded to instances (EventKit's shape); the window bounds
    /// only apply to full lists (the API forbids them with `syncToken`).
    fn list_events(
        &self,
        calendar_id: &str,
        q: &EventsQuery,
        win: &PassWindow,
    ) -> Result<EventsPage, FetchError> {
        let mut params = vec![
            ("maxResults", PAGE_SIZE.to_string()),
            ("singleEvents", "true".to_string()),
        ];
        match q {
            EventsQuery::Full { page_token } => {
                params.push(("timeMin", win.time_min.clone()));
                params.push(("timeMax", win.time_max.clone()));
                if let Some(t) = page_token {
                    params.push(("pageToken", t.clone()));
                }
            }
            EventsQuery::Incremental { sync_token, page_token } => {
                params.push(("syncToken", sync_token.clone()));
                if let Some(t) = page_token {
                    params.push(("pageToken", t.clone()));
                }
            }
        }
        let v = self.get(
            &format!("/calendar/v3/calendars/{}/events", encode_path_segment(calendar_id)),
            &params,
        )?;
        Ok(EventsPage {
            items: v.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
            next_page_token: v.get("nextPageToken").and_then(Value::as_str).map(str::to_string),
            next_sync_token: v.get("nextSyncToken").and_then(Value::as_str).map(str::to_string),
        })
    }
}

/// Percent-encode one URL path segment (calendar ids are email-shaped and
/// can carry `@` and `#`, e.g. `addressbook#contacts@group.v.calendar...`).
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Normalization: Google event JSON → store occurrences

/// One pass's window and timestamps, computed once so every calendar of
/// every account diffs against the same bounds.
struct PassWindow {
    /// Diff window bounds, day-granular (the store convention).
    ws: String,
    we: String,
    /// The same bounds as RFC3339 for `timeMin`/`timeMax`.
    time_min: String,
    time_max: String,
    /// The pass timestamp stamped onto change lines.
    ts: String,
    /// Today (YYYY-MM-DD), for the full-refresh staleness check.
    today: String,
}

impl PassWindow {
    fn at(now: chrono::DateTime<Local>) -> Self {
        let past = now - ChronoDuration::days(WINDOW_PAST_DAYS);
        let future = now + ChronoDuration::days(WINDOW_FUTURE_DAYS);
        PassWindow {
            ws: past.format("%Y-%m-%d").to_string(),
            we: future.format("%Y-%m-%d").to_string(),
            time_min: past.to_rfc3339(),
            time_max: future.to_rfc3339(),
            ts: now.to_rfc3339(),
            today: now.format("%Y-%m-%d").to_string(),
        }
    }
}

/// Everything identifying one calendar of one account during a pass.
struct CalContext {
    sub: String,
    account_email: String,
    cal_id: String,
    summary: String,
}

/// One normalized `events.list` item.
enum EventDelta {
    /// `status: "cancelled"` — drop one instance (`slot` set, from
    /// `originalStartTime`) or the whole event (`slot` `None`).
    Cancelled { series: String, row: String, slot: Option<String> },
    /// A live instance, ready for the store (or for suppression when its
    /// `uid` turns out to be EventKit-covered).
    Upsert { series: String, uid: String, occ: CalendarOccurrence },
}

/// The store row id for one event series: ownership prefix + account `sub`
/// + calendar id + series id (`/`-joined; none of the parts can contain
/// `/`).
fn row_id(sub: &str, cal_id: &str, series: &str) -> String {
    format!("{GOOGLE_ROW_PREFIX}{sub}/{cal_id}/{series}")
}

/// The id prefix owning every row of one calendar — the snapshot scope.
fn calendar_prefix(sub: &str, cal_id: &str) -> String {
    format!("{GOOGLE_ROW_PREFIX}{sub}/{cal_id}/")
}

/// The series id back out of a stored Google row id.
fn series_of(id: &str) -> Option<&str> {
    id.strip_prefix(GOOGLE_ROW_PREFIX)?.splitn(3, '/').nth(2)
}

/// The iCalUID Google mints for its own events. Imported events deviate and
/// are tracked in [`GcalCalendarState::uid_exceptions`].
fn conventional_uid(series: &str) -> String {
    format!("{series}@google.com")
}

/// The dedupe key for one series, re-derived from per-calendar state.
fn uid_of(cstate: &GcalCalendarState, series: &str) -> String {
    cstate
        .uid_exceptions
        .get(series)
        .cloned()
        .unwrap_or_else(|| conventional_uid(series))
}

/// The `calendarItemExternalIdentifier` (= iCalUID) embedded in an
/// EventKit-sourced row id — the substring between the first `:` and any
/// `/RID=` recurrence suffix. `None` for ids with no embedded identifier.
fn eventkit_external_id(id: &str) -> Option<String> {
    let (_, external) = id.split_once(':')?;
    let external = external.split("/RID=").next().unwrap_or(external);
    (!external.is_empty()).then(|| external.to_string())
}

/// A `start`/`end`/`originalStartTime` field → RFC3339 local (the store
/// convention): `dateTime` re-zoned, all-day `date` at local midnight.
fn time_field_local(v: &Value) -> Option<String> {
    if let Some(dt) = v.get("dateTime").and_then(Value::as_str) {
        return chrono::DateTime::parse_from_rfc3339(dt)
            .ok()
            .map(|t| t.with_timezone(&Local).to_rfc3339());
    }
    let d = v.get("date").and_then(Value::as_str)?;
    local_at(NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?, 0, 0, 0)
}

/// RFC3339 local at a wall-clock time on a day (DST gaps resolve earliest).
fn local_at(d: NaiveDate, h: u32, m: u32, s: u32) -> Option<String> {
    Local
        .from_local_datetime(&d.and_hms_opt(h, m, s)?)
        .earliest()
        .map(|t| t.to_rfc3339())
}

/// Google's exclusive-end all-day `[date, end.date)` → the store's
/// inclusive `T00:00:00` … `T23:59:59` bounds (the EventKit shape).
fn all_day_bounds(d0: &str, d1_exclusive: &str) -> Option<(String, String)> {
    let start = NaiveDate::parse_from_str(d0, "%Y-%m-%d").ok()?;
    let end_excl = NaiveDate::parse_from_str(d1_exclusive, "%Y-%m-%d").ok()?;
    let last = end_excl.pred_opt().unwrap_or(start).max(start);
    Some((local_at(start, 0, 0, 0)?, local_at(last, 23, 59, 59)?))
}

/// One raw `events.list` item → an [`EventDelta`], or `None` for items
/// missing the fields that make them representable (no id, no start).
fn normalize_event(item: &Value, ctx: &CalContext) -> Option<EventDelta> {
    let event_id = item.get("id").and_then(Value::as_str)?;
    // The series id: all instances of a recurring event share their
    // master's id, matching the store's "one id across occurrences" key.
    let series = item
        .get("recurringEventId")
        .and_then(Value::as_str)
        .unwrap_or(event_id)
        .to_string();
    let row = row_id(&ctx.sub, &ctx.cal_id, &series);
    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
    if status == "cancelled" {
        // Cancelled items arrive skeletal: an instance cancellation carries
        // `originalStartTime`, a whole-event cancellation doesn't.
        let slot = item.get("originalStartTime").and_then(time_field_local);
        return Some(EventDelta::Cancelled { series, row, slot });
    }

    let start_field = item.get("start")?;
    let all_day = start_field.get("date").is_some();
    let (start, end) = if all_day {
        let d0 = start_field.get("date").and_then(Value::as_str)?;
        let d1 = item
            .get("end")
            .and_then(|e| e.get("date"))
            .and_then(Value::as_str)
            .unwrap_or(d0);
        all_day_bounds(d0, d1)?
    } else {
        let s = time_field_local(start_field)?;
        let e = item.get("end").and_then(time_field_local).unwrap_or_else(|| s.clone());
        (s, e)
    };
    let recurring = item.get("recurringEventId").is_some() || item.get("recurrence").is_some();
    // The instance's original slot — stable across a reschedule, exactly
    // what makes `id + occurrence` a usable diff key. A non-recurring event
    // has no slot concept: leave it empty so the key stays stable when the
    // event moves and the diff reads `changed`, not `removed` + `added`.
    let occurrence = item
        .get("originalStartTime")
        .and_then(time_field_local)
        .unwrap_or_else(|| if recurring { start.clone() } else { String::new() });
    let uid = item
        .get("iCalUID")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| conventional_uid(&series));
    let text = |key: &str| {
        item.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
    };
    let attendees = item
        .get("attendees")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    p.get("displayName")
                        .or_else(|| p.get("email"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    Some(EventDelta::Upsert {
        series,
        uid,
        occ: CalendarOccurrence {
            id: row,
            occurrence,
            start,
            end,
            all_day,
            title: text("summary"),
            calendar: ctx.summary.clone(),
            account: ctx.account_email.clone(),
            location: text("location"),
            notes: text("description"),
            attendees,
            status: match status {
                "confirmed" => "confirmed".into(),
                "tentative" => "tentative".into(),
                _ => String::new(),
            },
            recurring,
        },
    })
}

// ---------------------------------------------------------------------------
// The dedupe and the per-calendar working set

/// Everything already covering events this account would otherwise write:
/// iCalUIDs embedded in EventKit-owned rows, plus series ids owned by
/// earlier-sorted connected accounts (shared-calendar dedupe).
#[derive(Default)]
struct Coverage {
    uids: HashSet<String>,
    other_series: HashSet<String>,
}

impl Coverage {
    fn covers(&self, series: &str, uid: &str) -> bool {
        self.uids.contains(uid) || self.other_series.contains(series)
    }
}

/// Fold one delta into a calendar's working set, suppressing (and
/// recording) events some other source already covers.
fn apply_delta(
    set: &mut BTreeMap<String, CalendarOccurrence>,
    cstate: &mut GcalCalendarState,
    cov: &Coverage,
    delta: EventDelta,
) {
    match delta {
        EventDelta::Cancelled { series, row, slot } => {
            match slot {
                Some(s) => {
                    set.remove(&format!("{row}/{s}"));
                }
                None => set.retain(|_, o| o.id != row),
            }
            // A cancelled covered event needs no further suppression — and
            // its EventKit row vanishing must not read as coverage loss.
            cstate.covered.remove(&series);
        }
        EventDelta::Upsert { series, uid, occ } => {
            if uid != conventional_uid(&series) {
                cstate.uid_exceptions.insert(series.clone(), uid.clone());
            } else {
                cstate.uid_exceptions.remove(&series);
            }
            if cov.covers(&series, &uid) {
                // Prefer not writing a duplicate over writing one: drop
                // every instance of the covered series, including any
                // already in the working set from disk.
                set.retain(|_, o| o.id != occ.id);
                let day = occ.day().to_string();
                let entry = cstate.covered.entry(series).or_default();
                if day > *entry {
                    *entry = day;
                }
            } else {
                cstate.covered.remove(&series);
                set.insert(occ.key(), occ);
            }
        }
    }
}

/// Catch rows already on disk whose event just *became* covered (the
/// account was added to macOS, so no Google-side delta arrives): hand them
/// off by dropping them from the working set. Incremental passes only —
/// full lists re-evaluate every event through [`apply_delta`] anyway.
fn sweep_gained_coverage(
    set: &mut BTreeMap<String, CalendarOccurrence>,
    cstate: &mut GcalCalendarState,
    cov: &Coverage,
) {
    let gained: Vec<(String, String, String)> = set
        .values()
        .filter_map(|o| {
            let series = series_of(&o.id)?;
            let uid = uid_of(cstate, series);
            cov.covers(series, &uid)
                .then(|| (o.key(), series.to_string(), o.day().to_string()))
        })
        .collect();
    for (key, series, day) in gained {
        set.remove(&key);
        let entry = cstate.covered.entry(series).or_default();
        if day > *entry {
            *entry = day;
        }
    }
}

/// What one calendar's pass did, rolled up into [`GcalSyncStats`].
#[derive(Default)]
struct CalPassStats {
    baselined: u64,
    added: u64,
    changed: u64,
    removed: u64,
}

impl Vault {
    /// One Google Calendar sync pass across every connected account:
    /// refresh each account's token, list its calendars, and bring each
    /// calendar's slice of the store current (full window list first, sync
    /// tokens after). A silent no-op when no Google account is connected.
    /// Per-account failures are recorded in the state file and never abort
    /// other accounts; state is persisted after every calendar, so an
    /// interrupted pass resumes with nothing lost. Blocking (network).
    pub fn collect_google_calendar(&self) -> Result<GcalSyncStats> {
        let accounts = self.google_status()?.accounts;
        let mut state = self.read_gcal_sync().unwrap_or_default();
        // Forget state for accounts that have been disconnected. Their
        // rows stay in the vault (data is never deleted on disconnect);
        // they simply freeze until the account reconnects.
        let live: HashSet<&str> = accounts.iter().map(|a| a.sub.as_str()).collect();
        state.accounts.retain(|sub, _| live.contains(sub.as_str()));

        if accounts.is_empty() {
            // Persist the pruning above so a disconnected account's rows
            // don't linger in the state or the index.
            if self.resolve(STATE_FILE).map(|p| p.exists()).unwrap_or(false) {
                state.updated = Local::now().to_rfc3339();
                self.write_gcal_sync(&state)?;
                self.write_gcal_index(&state)?;
            }
            return Ok(GcalSyncStats::default());
        }

        let win = PassWindow::at(Local::now());
        let mut stats = GcalSyncStats::default();

        for (i, acct) in accounts.iter().enumerate() {
            // A flagged account can't refresh non-interactively; skip it
            // (the card surfaces the reconnect prompt).
            if acct.needs_reconnect {
                continue;
            }
            let token = match crate::sync::google::fresh_token(self, &acct.sub) {
                Ok(t) => t.access_token,
                Err(e) => {
                    let astate = state.accounts.entry(acct.sub.clone()).or_default();
                    astate.email = acct.email.clone();
                    astate.error = Some(format!("{e:#}"));
                    continue;
                }
            };
            let client = GcalClient { base: GCAL_API.to_string(), token };
            // Accounts are sorted by email; for a calendar shared between
            // two connected accounts, the earlier-sorted one owns the rows
            // and the later defers (a stable rule, so ownership never
            // flaps between passes).
            let earlier_subs: HashSet<String> =
                accounts[..i].iter().map(|a| a.sub.clone()).collect();
            {
                let astate = state.accounts.entry(acct.sub.clone()).or_default();
                astate.email = acct.email.clone();
                astate.error = None;
            }
            match self.gcal_sync_account(&client, acct, &earlier_subs, &mut state, &win, &mut stats)
            {
                Ok(()) => stats.accounts += 1,
                Err(e) => {
                    let msg = format!("{}", status_error(&acct.email, e));
                    let astate = state.accounts.entry(acct.sub.clone()).or_default();
                    astate.error = Some(msg);
                }
            }
        }

        state.updated = win.ts.clone();
        self.write_gcal_sync(&state)?;
        self.write_gcal_index(&state)?;
        Ok(stats)
    }

    /// Pull every connected account's Google Calendar into the vault — the
    /// manual "Sync now" / first-connect path. Blocking (network).
    pub fn google_calendar_pull(&self) -> Result<GcalSyncStats> {
        if self.google_status()?.accounts.is_empty() {
            bail!("no Google account is connected");
        }
        self.collect_google_calendar()
    }

    /// One account's pass: list calendars, retire calendars that vanished,
    /// then sync each calendar, persisting state after every step.
    fn gcal_sync_account(
        &self,
        client: &GcalClient,
        acct: &GoogleAccountInfo,
        earlier_subs: &HashSet<String>,
        state: &mut GcalSyncState,
        win: &PassWindow,
        stats: &mut GcalSyncStats,
    ) -> Result<(), FetchError> {
        let cals = client.list_calendars()?;
        let cov = self.gcal_coverage(earlier_subs, win).map_err(soft)?;

        // Calendars that vanished from the account (deleted or
        // unsubscribed): their stored window rows drop with honest
        // `removed` change lines — the events really are gone from the
        // account's calendar set.
        let listed: HashSet<&str> = cals.iter().map(|c| c.id.as_str()).collect();
        let gone: Vec<String> = state
            .accounts
            .get(&acct.sub)
            .map(|a| {
                a.calendars.keys().filter(|k| !listed.contains(k.as_str())).cloned().collect()
            })
            .unwrap_or_default();
        for cal_id in gone {
            let prefix = calendar_prefix(&acct.sub, &cal_id);
            let s = self
                .calendar_snapshot_scoped(&[], &win.ws, &win.we, &win.ts, &|o| {
                    o.id.starts_with(&prefix)
                })
                .map_err(soft)?;
            stats.removed += s.removed;
            if let Some(a) = state.accounts.get_mut(&acct.sub) {
                a.calendars.remove(&cal_id);
            }
            self.write_gcal_sync(state).map_err(soft)?;
        }

        for cal in &cals {
            let first = state
                .accounts
                .get(&acct.sub)
                .map_or(true, |a| !a.calendars.contains_key(&cal.id));
            // Work on a copy: a mid-calendar failure leaves the persisted
            // state exactly where the last completed calendar put it.
            let mut cstate = state
                .accounts
                .get(&acct.sub)
                .and_then(|a| a.calendars.get(&cal.id))
                .cloned()
                .unwrap_or_default();
            cstate.summary = cal.summary.clone();
            let ctx = CalContext {
                sub: acct.sub.clone(),
                account_email: acct.email.clone(),
                cal_id: cal.id.clone(),
                summary: cal.summary.clone(),
            };
            let mut fetch = |q: &EventsQuery| client.list_events(&cal.id, q, win);
            let s = self.gcal_sync_calendar(&mut fetch, &ctx, &cov, &mut cstate, first, win)?;
            stats.calendars += 1;
            stats.baselined += s.baselined;
            stats.added += s.added;
            stats.changed += s.changed;
            stats.removed += s.removed;
            let astate = state.accounts.entry(acct.sub.clone()).or_default();
            astate.calendars.insert(cal.id.clone(), cstate);
            // Atomic write after each calendar — the resumability point.
            self.write_gcal_sync(state).map_err(soft)?;
        }
        Ok(())
    }

    /// Bring one calendar's slice of the store current: decide full vs
    /// incremental, fetch, fold deltas into the working set (suppressing
    /// EventKit-covered events), and snapshot the result through the shared
    /// diff machinery. `fetch` is injected so tests can script responses.
    fn gcal_sync_calendar(
        &self,
        fetch: &mut dyn FnMut(&EventsQuery) -> Result<EventsPage, FetchError>,
        ctx: &CalContext,
        cov: &Coverage,
        cstate: &mut GcalCalendarState,
        first: bool,
        win: &PassWindow,
    ) -> Result<CalPassStats, FetchError> {
        // Covered entries whose last seen instance scrolled out of the
        // window can no longer be verified against the store — drop them
        // (the monthly full refresh re-establishes any that still matter).
        cstate.covered.retain(|_, day| day.as_str() >= win.ws.as_str());
        // Coverage loss: a covered event's EventKit row is gone (the
        // account was removed from macOS). Sync tokens won't re-deliver an
        // unchanged event, so only a full re-list can restore it.
        let coverage_lost =
            cstate.covered.keys().any(|series| !cov.covers(series, &uid_of(cstate, series)));
        let stale = NaiveDate::parse_from_str(&cstate.full_synced, "%Y-%m-%d")
            .ok()
            .zip(NaiveDate::parse_from_str(&win.today, "%Y-%m-%d").ok())
            .map(|(full, today)| (today - full).num_days() >= FULL_REFRESH_DAYS)
            .unwrap_or(true);
        let mut full = first || cstate.sync_token.is_none() || coverage_lost || stale;

        let prefix = calendar_prefix(&ctx.sub, &ctx.cal_id);
        let stored: Vec<CalendarOccurrence> = self
            .calendar_range(&win.ws, &win.we)
            .map_err(soft)?
            .into_iter()
            .filter(|o| o.id.starts_with(&prefix))
            .collect();

        // The working set: a full list rebuilds from scratch (so events
        // deleted while the token was dead vanish), an incremental starts
        // from the stored mirror and folds the deltas in.
        let mut set: BTreeMap<String, CalendarOccurrence> = if full {
            BTreeMap::new()
        } else {
            stored.iter().map(|o| (o.key(), o.clone())).collect()
        };
        if full {
            cstate.covered.clear();
            cstate.uid_exceptions.clear();
        }

        let mut page_token: Option<String> = None;
        let mut new_sync_token: Option<String> = None;
        loop {
            let q = if full {
                EventsQuery::Full { page_token: page_token.clone() }
            } else {
                EventsQuery::Incremental {
                    sync_token: cstate.sync_token.clone().unwrap_or_default(),
                    page_token: page_token.clone(),
                }
            };
            let page = match fetch(&q) {
                Ok(p) => p,
                // 410 GONE: the token expired server-side — reset and do
                // the full window re-list right now.
                Err(FetchError::SyncGone) if !full => {
                    full = true;
                    set.clear();
                    cstate.covered.clear();
                    cstate.uid_exceptions.clear();
                    page_token = None;
                    continue;
                }
                Err(e) => return Err(e),
            };
            for item in &page.items {
                if let Some(delta) = normalize_event(item, ctx) {
                    apply_delta(&mut set, cstate, cov, delta);
                }
            }
            if let Some(t) = page.next_sync_token {
                new_sync_token = Some(t);
            }
            match page.next_page_token {
                Some(t) => page_token = Some(t),
                None => break,
            }
        }
        if !full {
            sweep_gained_coverage(&mut set, cstate, cov);
        }

        // Clamp to the diff window (instances straddling the fetch bounds
        // can poke past it) — the same clamp the EventKit pass applies.
        let mut fresh: Vec<CalendarOccurrence> = set
            .into_values()
            .filter(|o| o.day() >= win.ws.as_str() && o.day() <= win.we.as_str())
            .collect();
        fresh.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.key().cmp(&b.key())));

        let out = if first && stored.is_empty() {
            // First-ever sync of this calendar: years of existing events
            // are state, not "added" events — merge silently, no change
            // lines (the EventKit baseline doctrine).
            self.calendar_backfill(&fresh).map_err(soft)?;
            CalPassStats { baselined: fresh.len() as u64, ..Default::default() }
        } else {
            let s = self
                .calendar_snapshot_scoped(&fresh, &win.ws, &win.we, &win.ts, &|o| {
                    o.id.starts_with(&prefix)
                })
                .map_err(soft)?;
            CalPassStats { added: s.added, changed: s.changed, removed: s.removed, baselined: 0 }
        };
        cstate.sync_token = new_sync_token;
        if full {
            cstate.full_synced = win.today.clone();
        }
        cstate.events = fresh.len() as u64;
        Ok(out)
    }

    /// Scan the stored window for everything that covers this account's
    /// events: iCalUIDs embedded in EventKit-owned rows, and series ids
    /// already owned by earlier-sorted connected accounts.
    fn gcal_coverage(&self, earlier_subs: &HashSet<String>, win: &PassWindow) -> Result<Coverage> {
        let mut cov = Coverage::default();
        for row in self.calendar_range(&win.ws, &win.we)? {
            if let Some(rest) = row.id.strip_prefix(GOOGLE_ROW_PREFIX) {
                let mut parts = rest.splitn(3, '/');
                let sub = parts.next().unwrap_or_default();
                let _cal = parts.next();
                if let Some(series) = parts.next() {
                    if earlier_subs.contains(sub) {
                        cov.other_series.insert(series.to_string());
                    }
                }
            } else if let Some(uid) = eventkit_external_id(&row.id) {
                cov.uids.insert(uid);
            }
        }
        Ok(cov)
    }

    /// The persisted Google Calendar sync progress, if a sync has ever run.
    pub fn read_gcal_sync(&self) -> Option<GcalSyncState> {
        let path = self.resolve(STATE_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Atomic write so readers never see a torn file — called after every
    /// calendar, which is what makes a pass resumable.
    fn write_gcal_sync(&self, state: &GcalSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(STATE_FILE)?, state)
    }

    /// Regenerate the human-readable summary at `calendar/google.md`.
    fn write_gcal_index(&self, state: &GcalSyncState) -> Result<()> {
        let mut md = format!(
            "# Google Calendar\n\nLast sync: {}\n\n| Account | Calendar | Events | Deduped (EventKit) | Last full list | Error |\n|---|---|---|---|---|---|\n",
            state.updated
        );
        for a in state.accounts.values() {
            if let Some(e) = &a.error {
                md.push_str(&format!("| {} | — | — | — | — | {e} |\n", a.email));
            }
            for c in a.calendars.values() {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} | |\n",
                    a.email,
                    c.summary,
                    c.events,
                    c.covered.len(),
                    if c.full_synced.is_empty() { "—" } else { &c.full_synced },
                ));
            }
        }
        crate::store::write_atomic(&self.resolve(INDEX_FILE)?, md.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use serde_json::json;

    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-gcal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn ctx() -> CalContext {
        CalContext {
            sub: "111".into(),
            account_email: "me@gmail.com".into(),
            cal_id: "me@gmail.com".into(),
            summary: "Personal".into(),
        }
    }

    /// RFC3339 local at `hour:00` `days` from now — always in the window.
    fn at(days: i64, hour: u32) -> String {
        local_at((Local::now() + ChronoDuration::days(days)).date_naive(), hour, 0, 0).unwrap()
    }

    fn timed_item(id: &str, start: &str, end: &str, summary: &str) -> Value {
        json!({
            "id": id,
            "status": "confirmed",
            "summary": summary,
            "iCalUID": format!("{id}@google.com"),
            "start": { "dateTime": start },
            "end": { "dateTime": end },
        })
    }

    fn upsert(delta: EventDelta) -> (String, String, CalendarOccurrence) {
        match delta {
            EventDelta::Upsert { series, uid, occ } => (series, uid, occ),
            EventDelta::Cancelled { .. } => panic!("expected an upsert"),
        }
    }

    /// Scripted fetch: pops canned responses, records whether each query
    /// was a full list.
    fn scripted(
        responses: Vec<Result<EventsPage, FetchError>>,
        log: Rc<RefCell<Vec<bool>>>,
    ) -> impl FnMut(&EventsQuery) -> Result<EventsPage, FetchError> {
        let mut queue = VecDeque::from(responses);
        move |q: &EventsQuery| {
            log.borrow_mut().push(matches!(q, EventsQuery::Full { .. }));
            queue.pop_front().expect("scripted fetch ran out of responses")
        }
    }

    fn page(items: Vec<Value>, sync_token: &str) -> Result<EventsPage, FetchError> {
        Ok(EventsPage {
            items,
            next_page_token: None,
            next_sync_token: Some(sync_token.to_string()),
        })
    }

    #[test]
    fn normalize_timed_event_converts_and_keys() {
        let mut item = timed_item("ev1", &at(1, 10), &at(1, 11), "Standup");
        item["location"] = json!("Room 1");
        item["description"] = json!("agenda");
        item["attendees"] = json!([
            { "email": "a@x.com", "displayName": "Alice" },
            { "email": "b@x.com" },
        ]);
        item["status"] = json!("tentative");
        let (series, uid, occ) = upsert(normalize_event(&item, &ctx()).unwrap());
        assert_eq!(series, "ev1");
        assert_eq!(uid, "ev1@google.com");
        assert_eq!(occ.id, "gcal:111/me@gmail.com/ev1");
        assert!(
            occ.occurrence.is_empty(),
            "non-recurring events have no slot — the key stays stable across moves"
        );
        // The start survives the to-local round trip as the same instant.
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&occ.start).unwrap(),
            chrono::DateTime::parse_from_rfc3339(&at(1, 10)).unwrap()
        );
        assert!(!occ.all_day);
        assert_eq!(occ.title, "Standup");
        assert_eq!(occ.calendar, "Personal");
        assert_eq!(occ.account, "me@gmail.com");
        assert_eq!(occ.location, "Room 1");
        assert_eq!(occ.notes, "agenda");
        assert_eq!(occ.attendees, vec!["Alice", "b@x.com"]);
        assert_eq!(occ.status, "tentative");
        assert!(!occ.recurring);
    }

    #[test]
    fn normalize_recurring_instance_keys_by_series_and_slot() {
        let mut item = timed_item("abc_20260615T160000Z", &at(2, 11), &at(2, 12), "Daily");
        item["recurringEventId"] = json!("abc");
        item["iCalUID"] = json!("abc@google.com");
        item["originalStartTime"] = json!({ "dateTime": at(2, 9) });
        let (series, uid, occ) = upsert(normalize_event(&item, &ctx()).unwrap());
        assert_eq!(series, "abc", "instances key by their master");
        assert_eq!(uid, "abc@google.com");
        assert_eq!(occ.id, "gcal:111/me@gmail.com/abc");
        // The occurrence is the *original* slot, stable across the move.
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&occ.occurrence).unwrap(),
            chrono::DateTime::parse_from_rfc3339(&at(2, 9)).unwrap()
        );
        assert!(occ.recurring);
    }

    #[test]
    fn normalize_all_day_event_uses_inclusive_store_bounds() {
        // Google's all-day end date is exclusive; the store wants the last
        // day at 23:59:59 (the EventKit shape).
        let item = json!({
            "id": "trip",
            "status": "confirmed",
            "summary": "Trip",
            "start": { "date": "2026-07-01" },
            "end": { "date": "2026-07-03" },
        });
        let (_, uid, occ) = upsert(normalize_event(&item, &ctx()).unwrap());
        assert_eq!(uid, "trip@google.com", "missing iCalUID falls back to convention");
        assert!(occ.all_day);
        assert!(occ.start.starts_with("2026-07-01T00:00:00"), "start={}", occ.start);
        assert!(occ.end.starts_with("2026-07-02T23:59:59"), "end={}", occ.end);
    }

    #[test]
    fn normalize_cancelled_items() {
        // A cancelled instance carries its original slot…
        let inst = json!({
            "id": "abc_20260615T160000Z",
            "status": "cancelled",
            "recurringEventId": "abc",
            "originalStartTime": { "dateTime": at(3, 9) },
        });
        match normalize_event(&inst, &ctx()).unwrap() {
            EventDelta::Cancelled { series, row, slot } => {
                assert_eq!(series, "abc");
                assert_eq!(row, "gcal:111/me@gmail.com/abc");
                assert_eq!(
                    chrono::DateTime::parse_from_rfc3339(&slot.unwrap()).unwrap(),
                    chrono::DateTime::parse_from_rfc3339(&at(3, 9)).unwrap()
                );
            }
            EventDelta::Upsert { .. } => panic!("expected a cancellation"),
        }
        // …a whole-event cancellation doesn't (slot None → drop the event).
        let whole = json!({ "id": "xyz", "status": "cancelled" });
        match normalize_event(&whole, &ctx()).unwrap() {
            EventDelta::Cancelled { slot, .. } => assert!(slot.is_none()),
            EventDelta::Upsert { .. } => panic!("expected a cancellation"),
        }
    }

    #[test]
    fn eventkit_external_id_extraction_matches_real_formats() {
        // Real formats observed on disk: Google Workspace via CalDAV…
        assert_eq!(
            eventkit_external_id(
                "AB2C4FD5-1486-407E-97E6-21CECAA1D03E:2cg115mfr3nr2634ok2nqiodl9_R20260302T180000@google.com/RID=802026000"
            ).as_deref(),
            Some("2cg115mfr3nr2634ok2nqiodl9_R20260302T180000@google.com")
        );
        // …an Apple-created event in a Google calendar (UUID iCalUID)…
        assert_eq!(
            eventkit_external_id(
                "CEB3F3FA-1111-4970-B188-CE3C308DC56F:84678F0C-4A92-4F7C-9AFF-E37C4C48421C"
            )
            .as_deref(),
            Some("84678F0C-4A92-4F7C-9AFF-E37C4C48421C")
        );
        // …and an id with no embedded identifier at all.
        assert_eq!(eventkit_external_id("PLAIN-UUID-NO-COLON"), None);
    }

    #[test]
    fn apply_delta_dedupes_and_cancels() {
        let mut set = BTreeMap::new();
        let mut cstate = GcalCalendarState::default();
        let mut cov = Coverage::default();
        cov.uids.insert("covered1@google.com".into());

        // A covered event is suppressed and recorded…
        let covered = normalize_event(
            &timed_item("covered1", &at(1, 10), &at(1, 11), "Already via EventKit"),
            &ctx(),
        )
        .unwrap();
        apply_delta(&mut set, &mut cstate, &cov, covered);
        assert!(set.is_empty());
        assert!(cstate.covered.contains_key("covered1"));

        // …an uncovered one lands in the working set…
        let live = normalize_event(&timed_item("ev2", &at(1, 13), &at(1, 14), "Mine"), &ctx())
            .unwrap();
        apply_delta(&mut set, &mut cstate, &cov, live);
        assert_eq!(set.len(), 1);

        // …and a whole-event cancellation removes it again.
        let cancel = normalize_event(&json!({ "id": "ev2", "status": "cancelled" }), &ctx())
            .unwrap();
        apply_delta(&mut set, &mut cstate, &cov, cancel);
        assert!(set.is_empty());
    }

    #[test]
    fn nonstandard_uid_is_tracked_and_used_for_dedupe() {
        let mut set = BTreeMap::new();
        let mut cstate = GcalCalendarState::default();
        let mut cov = Coverage::default();
        // An imported event whose iCalUID is a UUID, not {id}@google.com —
        // covered by an EventKit row carrying that same UUID.
        cov.uids.insert("84678F0C-4A92-4F7C-9AFF-E37C4C48421C".into());
        let mut item = timed_item("imported1", &at(1, 10), &at(1, 11), "Imported");
        item["iCalUID"] = json!("84678F0C-4A92-4F7C-9AFF-E37C4C48421C");
        let delta = normalize_event(&item, &ctx()).unwrap();
        apply_delta(&mut set, &mut cstate, &cov, delta);
        assert!(set.is_empty(), "deduped on the real iCalUID, not the convention");
        assert_eq!(
            cstate.uid_exceptions.get("imported1").map(String::as_str),
            Some("84678F0C-4A92-4F7C-9AFF-E37C4C48421C"),
            "the exception is remembered so disk rows can re-derive the key"
        );
        assert_eq!(uid_of(&cstate, "imported1"), "84678F0C-4A92-4F7C-9AFF-E37C4C48421C");
    }

    /// EventKit-style seed row whose embedded external id is `uid`.
    fn ek_occ(uid: &str, start: &str, end: &str, title: &str) -> CalendarOccurrence {
        CalendarOccurrence {
            id: format!("EK1A2B3C-0000:{uid}"),
            occurrence: start.into(),
            start: start.into(),
            end: end.into(),
            all_day: false,
            title: title.into(),
            calendar: "Personal".into(),
            account: "Gmail".into(),
            location: String::new(),
            notes: String::new(),
            attendees: Vec::new(),
            status: "confirmed".into(),
            recurring: false,
        }
    }

    #[test]
    fn first_sync_baselines_silently_and_dedupes_against_eventkit() {
        let v = temp_vault("dedupe");
        let win = PassWindow::at(Local::now());
        // EventKit already syncs this Google account: one event on disk.
        let ek = ek_occ("dup1@google.com", &at(1, 10), &at(1, 11), "Standup");
        v.calendar_snapshot(&[ek.clone()], &win.ws, &win.we, &win.ts).unwrap();

        let cov = v.gcal_coverage(&HashSet::new(), &win).unwrap();
        assert!(cov.uids.contains("dup1@google.com"));

        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![page(
                vec![
                    timed_item("dup1", &at(1, 10), &at(1, 11), "Standup"),
                    timed_item("only-google", &at(2, 9), &at(2, 10), "Not in macOS"),
                ],
                "T1",
            )],
            log.clone(),
        );
        let mut cstate = GcalCalendarState::default();
        let s = v
            .gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, true, &win)
            .unwrap();
        assert_eq!(*log.borrow(), vec![true], "first sync is one full list");
        assert_eq!(s.baselined, 1, "only the uncovered event is written");
        assert_eq!(s.added + s.changed + s.removed, 0);

        let rows = v.calendar_range(&win.ws, &win.we).unwrap();
        let gcal: Vec<_> = rows.iter().filter(|o| o.id.starts_with("gcal:")).collect();
        assert_eq!(gcal.len(), 1, "the EventKit-covered event was not doubled");
        assert_eq!(gcal[0].id, "gcal:111/me@gmail.com/only-google");
        assert!(cstate.covered.contains_key("dup1"));
        assert_eq!(cstate.sync_token.as_deref(), Some("T1"));
        assert_eq!(cstate.full_synced, win.today);
        assert!(
            !v.root().join("calendar/changes").exists(),
            "baseline writes no change lines"
        );

        // The next EventKit pass must not wipe the Google-owned row.
        v.calendar_snapshot(&[ek], &win.ws, &win.we, &win.ts).unwrap();
        let rows = v.calendar_range(&win.ws, &win.we).unwrap();
        assert!(
            rows.iter().any(|o| o.id == "gcal:111/me@gmail.com/only-google"),
            "scoped ownership keeps the Google row through an EventKit snapshot"
        );
        assert!(
            !v.root().join("calendar/changes").exists(),
            "and the EventKit diff doesn't log it as removed"
        );
    }

    #[test]
    fn incremental_reschedule_and_cancellation_flow_to_changes() {
        let v = temp_vault("incremental");
        let win = PassWindow::at(Local::now());
        let cov = Coverage::default();
        let mut cstate = GcalCalendarState::default();

        // Pass 1: silent baseline of one event.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![page(vec![timed_item("ev1", &at(1, 9), &at(1, 10), "Mtg")], "T1")],
            log.clone(),
        );
        v.gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, true, &win).unwrap();

        // Pass 2: the event moved → one `changed` line with the start drift.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![page(vec![timed_item("ev1", &at(1, 14), &at(1, 15), "Mtg")], "T2")],
            log.clone(),
        );
        let s = v
            .gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, false, &win)
            .unwrap();
        assert_eq!(*log.borrow(), vec![false], "second pass rides the sync token");
        assert_eq!((s.added, s.changed, s.removed), (0, 1, 0));
        assert_eq!(cstate.sync_token.as_deref(), Some("T2"));
        let changes = v.calendar_changes(&win.ws, &win.we).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "changed");
        assert!(changes[0].changes.iter().any(|c| c.field == "start"));

        // Pass 3: cancelled → one `removed` line.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![page(vec![json!({ "id": "ev1", "status": "cancelled" })], "T3")],
            log.clone(),
        );
        let s = v
            .gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, false, &win)
            .unwrap();
        assert_eq!((s.added, s.changed, s.removed), (0, 0, 1));
        let changes = v.calendar_changes(&win.ws, &win.we).unwrap();
        assert_eq!(changes.last().unwrap().kind, "removed");
    }

    #[test]
    fn expired_sync_token_resets_to_a_full_relist() {
        let v = temp_vault("gone");
        let win = PassWindow::at(Local::now());
        let cov = Coverage::default();
        let mut cstate = GcalCalendarState::default();

        // Baseline with two events.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![page(
                vec![
                    timed_item("keep", &at(1, 9), &at(1, 10), "Keep"),
                    timed_item("dropped", &at(2, 9), &at(2, 10), "Dropped while away"),
                ],
                "T1",
            )],
            log.clone(),
        );
        v.gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, true, &win).unwrap();

        // The token aged out: 410 on the incremental, then a full re-list
        // that no longer contains `dropped`.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![
                Err(FetchError::SyncGone),
                page(vec![timed_item("keep", &at(1, 9), &at(1, 10), "Keep")], "T2"),
            ],
            log.clone(),
        );
        let s = v
            .gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, false, &win)
            .unwrap();
        assert_eq!(*log.borrow(), vec![false, true], "410 on the token, then a full list");
        assert_eq!((s.added, s.changed, s.removed), (0, 0, 1));
        assert_eq!(cstate.sync_token.as_deref(), Some("T2"));
        assert_eq!(cstate.full_synced, win.today);
        let rows = v.calendar_range(&win.ws, &win.we).unwrap();
        assert_eq!(rows.len(), 1, "the re-list replaced the mirror");
        let changes = v.calendar_changes(&win.ws, &win.we).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "removed");
        assert_eq!(changes[0].title, "Dropped while away");
    }

    #[test]
    fn coverage_loss_forces_a_full_relist_that_restores_the_event() {
        let v = temp_vault("covloss");
        let win = PassWindow::at(Local::now());
        // A populated store (an unrelated iCloud event) — the realistic
        // shape: coverage loss can only happen to a vault EventKit has
        // already written to, so this is no first-ever baseline.
        let icloud = ek_occ("UNRELATED-UUID", &at(4, 9), &at(4, 10), "iCloud thing");
        v.calendar_snapshot(&[icloud.clone()], &win.ws, &win.we, &win.ts).unwrap();
        // The Google event was covered by EventKit, but its row is gone now
        // (the account was removed from macOS) — it's not in the coverage
        // scan anymore.
        let cov = v.gcal_coverage(&HashSet::new(), &win).unwrap();
        assert!(!cov.uids.contains("wascovered@google.com"));
        let mut cstate = GcalCalendarState {
            sync_token: Some("T1".into()),
            full_synced: win.today.clone(),
            ..Default::default()
        };
        cstate.covered.insert("wascovered".into(), win.today.clone());

        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![page(
                vec![timed_item("wascovered", &at(1, 10), &at(1, 11), "Back to Google")],
                "T2",
            )],
            log.clone(),
        );
        let s = v
            .gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, false, &win)
            .unwrap();
        assert_eq!(*log.borrow(), vec![true], "lost coverage must force a full re-list");
        assert_eq!(s.added, 1, "the event is restored as a Google-owned row");
        assert!(cstate.covered.is_empty());
        let rows = v.calendar_range(&win.ws, &win.we).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().any(|o| o.id == "gcal:111/me@gmail.com/wascovered"),
            "the once-covered event is back in the store"
        );
    }

    #[test]
    fn out_of_window_covered_entries_prune_without_forcing_full() {
        let v = temp_vault("covprune");
        let win = PassWindow::at(Local::now());
        let cov = Coverage::default();
        let mut cstate = GcalCalendarState {
            sync_token: Some("T1".into()),
            full_synced: win.today.clone(),
            ..Default::default()
        };
        // A covered entry whose last instance long since left the window:
        // unverifiable, so it's pruned rather than read as coverage loss.
        cstate.covered.insert("ancient".into(), "2000-01-01".into());

        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(vec![page(vec![], "T2")], log.clone());
        v.gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, false, &win).unwrap();
        assert_eq!(*log.borrow(), vec![false], "stayed incremental");
        assert!(cstate.covered.is_empty(), "the stale entry was pruned");
    }

    #[test]
    fn gained_coverage_hands_stored_rows_off_to_eventkit() {
        let v = temp_vault("covgain");
        let win = PassWindow::at(Local::now());
        let mut cstate = GcalCalendarState::default();

        // Pass 1 (no EventKit): the event lands as a Google row.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(
            vec![page(vec![timed_item("ev1", &at(1, 9), &at(1, 10), "Mtg")], "T1")],
            log.clone(),
        );
        v.gcal_sync_calendar(&mut fetch, &ctx(), &Coverage::default(), &mut cstate, true, &win)
            .unwrap();

        // The account gets added to macOS: an EventKit row for the same
        // iCalUID appears. The next (empty) incremental hands the row off.
        let ek = ek_occ("ev1@google.com", &at(1, 9), &at(1, 10), "Mtg");
        v.calendar_snapshot(&[ek], &win.ws, &win.we, &win.ts).unwrap();
        let cov = v.gcal_coverage(&HashSet::new(), &win).unwrap();
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut fetch = scripted(vec![page(vec![], "T2")], log.clone());
        let s = v
            .gcal_sync_calendar(&mut fetch, &ctx(), &cov, &mut cstate, false, &win)
            .unwrap();
        assert_eq!(s.removed, 1, "the Google copy retires (EventKit owns it now)");
        assert!(cstate.covered.contains_key("ev1"));
        let rows = v.calendar_range(&win.ws, &win.we).unwrap();
        assert_eq!(rows.len(), 1, "exactly one copy remains");
        assert!(!rows[0].id.starts_with("gcal:"), "and it's the EventKit one");
    }

    #[test]
    fn collect_without_an_account_is_a_silent_noop() {
        let v = temp_vault("noaccount");
        let stats = v.collect_google_calendar().unwrap();
        assert_eq!(stats.calendars, 0);
        assert_eq!(stats.baselined + stats.added + stats.changed + stats.removed, 0);
        assert!(v.read_gcal_sync().is_none(), "no state file appears");
        assert!(!v.root().join(INDEX_FILE).exists(), "no index appears");
    }

    #[test]
    fn pull_without_an_account_is_a_clean_error() {
        // Unlike the silent scheduled pass, a user-triggered pull must say
        // why nothing happened.
        let v = temp_vault("pull-noaccount");
        let err = pull(&v).unwrap_err();
        assert!(err.to_string().contains("no Google account"), "{err}");
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        assert!(v.read_gcal_sync().is_none());
        let mut state = GcalSyncState {
            updated: "2026-06-12T10:00:00-07:00".into(),
            ..Default::default()
        };
        let mut cal = GcalCalendarState {
            summary: "Personal".into(),
            sync_token: Some("tok".into()),
            full_synced: "2026-06-12".into(),
            events: 42,
            ..Default::default()
        };
        cal.covered.insert("abc".into(), "2026-06-20".into());
        cal.uid_exceptions.insert("imp".into(), "UUID-1".into());
        let mut acct = GcalAccountState { email: "me@gmail.com".into(), ..Default::default() };
        acct.calendars.insert("me@gmail.com".into(), cal);
        state.accounts.insert("111".into(), acct);
        v.write_gcal_sync(&state).unwrap();

        let loaded = v.read_gcal_sync().unwrap();
        let a = &loaded.accounts["111"];
        assert_eq!(a.email, "me@gmail.com");
        let c = &a.calendars["me@gmail.com"];
        assert_eq!(c.sync_token.as_deref(), Some("tok"));
        assert_eq!(c.full_synced, "2026-06-12");
        assert_eq!(c.covered["abc"], "2026-06-20");
        assert_eq!(c.uid_exceptions["imp"], "UUID-1");
        assert_eq!(c.events, 42);
    }

    #[test]
    fn index_lists_calendars_and_errors() {
        let v = temp_vault("index");
        let mut state = GcalSyncState {
            updated: "2026-06-12T10:00:00-07:00".into(),
            ..Default::default()
        };
        let mut good = GcalAccountState { email: "a@gmail.com".into(), ..Default::default() };
        good.calendars.insert(
            "a@gmail.com".into(),
            GcalCalendarState {
                summary: "Personal".into(),
                full_synced: "2026-06-12".into(),
                events: 7,
                ..Default::default()
            },
        );
        state.accounts.insert("1".into(), good);
        state.accounts.insert(
            "2".into(),
            GcalAccountState {
                email: "b@gmail.com".into(),
                error: Some("rate limited".into()),
                ..Default::default()
            },
        );
        v.write_gcal_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join(INDEX_FILE)).unwrap();
        assert!(md.contains("| a@gmail.com | Personal | 7 | 0 | 2026-06-12 | |"));
        assert!(md.contains("| b@gmail.com | — | — | — | — | rate limited |"));
    }

    #[test]
    fn path_segment_encoding_covers_calendar_ids() {
        assert_eq!(encode_path_segment("me@gmail.com"), "me%40gmail.com");
        assert_eq!(
            encode_path_segment("addressbook#contacts@group.v.calendar.google.com"),
            "addressbook%23contacts%40group.v.calendar.google.com"
        );
    }
}
