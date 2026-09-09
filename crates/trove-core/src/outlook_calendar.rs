//! Microsoft Outlook Calendar collector — events from every connected Microsoft
//! account into the same `calendar/events/` store as EventKit and Google
//! Calendar, using the Microsoft Graph calendarView delta API for incremental
//! sync.
//!
//! **Target.** Occurrences land in the unified calendar snapshot
//! ([`crate::calendar`]) with ids of the form `mcal:{account_id}/{event_id}`.
//! The `mcal:` prefix ([`crate::calendar::MICROSOFT_ROW_PREFIX`]) is the
//! ownership marker that lets the three collectors (EventKit, Google, Outlook)
//! share one store: each snapshot pass diffs and rewrites only its own rows via
//! [`Vault::calendar_snapshot_scoped`].
//!
//! **API.** Uses `GET /me/calendarView/delta?startDateTime=…&endDateTime=…`
//! (beta note: the non-windowed `/me/events/delta` is beta-only; we use the
//! calendarView delta which is v1.0 stable). This expands recurring event
//! occurrences, matching EventKit's shape. Deleted events carry `@removed`.
//! The `@odata.deltaLink` is persisted per account after a complete drain;
//! the next pass supplies it instead of the window params.
//!
//! **State.** `.trove/outlook-calendar-sync.json` — per-account delta cursor
//! + counters; written after each complete drain so an interrupted pass
//! resumes. Per-account failures are recorded there and never abort other
//! accounts.
//!
//! **Raw layer.** Full-fidelity Graph event JSON is also appended to
//! `calendar/outlook/raw/YYYY-MM.jsonl` (keyed on event start time), so the
//! original response is preserved irrespective of the contract shape.
//!
//! **Auth.** Reuses the shared `microsoft` connection ([`crate::outlook`]).
//! The `Calendars.Read` scope is on the `MICROSOFT` provider; the calendar
//! pull uses the same per-account token store as the email pull.

use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{Duration as ChronoDuration, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::calendar::{
    CalendarOccurrence, MICROSOFT_ROW_PREFIX, WINDOW_FUTURE_DAYS, WINDOW_PAST_DAYS,
};
use crate::integrations::{Integration, IntegrationKind};
use crate::outlook::{microsoft_accounts, microsoft_fresh_token};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

/// Seconds between Microsoft Calendar passes — same cadence as Google Calendar
/// and Outlook email (cheap delta call when nothing has changed).
pub const MCAL_SYNC_SECS: u64 = 900;

const STATE_FILE: &str = ".trove/outlook-calendar-sync.json";
const RAW_DIR: &str = "calendar/outlook/raw";
const INDEX_FILE: &str = "calendar/outlook.md";
const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// State

/// Per-account sync state, persisted in the state file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McalAccountState {
    /// The account's email address (display only).
    #[serde(default)]
    pub email: String,
    /// The `@odata.deltaLink` from the last completed drain; `None` forces a
    /// fresh windowed calendarView delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_link: Option<String>,
    /// Occurrences currently written for this account (drives the index).
    #[serde(default)]
    pub events: u64,
    /// Why this account's last pass failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The whole Outlook Calendar sync state, persisted at
/// `.trove/outlook-calendar-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McalSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Per-account progress, keyed by the Microsoft account id (the Graph
    /// `id` field, same key as the token store).
    pub accounts: BTreeMap<String, McalAccountState>,
}

/// Stats from one sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default)]
pub struct McalSyncStats {
    pub accounts: u32,
    pub baselined: u64,
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
}

// ---------------------------------------------------------------------------
// API client

/// Status-level fetch errors.
#[derive(Debug)]
enum FetchError {
    RateLimited,
    Unauthorized,
    /// The delta token expired (HTTP 410) — restart with a fresh window.
    DeltaExpired,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429/403)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::DeltaExpired => write!(f, "delta token expired (HTTP 410)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Vault write error inside the fetch path → a soft error.
fn soft(e: anyhow::Error) -> FetchError {
    FetchError::Other(format!("{e:#}"))
}

/// Map a status-level error to a user-facing message.
fn status_message(account: &str, e: FetchError) -> String {
    match e {
        FetchError::RateLimited => {
            format!("Microsoft Calendar rate limited the sync ({account}) — it resumes next pass")
        }
        FetchError::Unauthorized => format!(
            "Microsoft Calendar rejected the token ({account}, 401) — reconnect from the Integrations tab"
        ),
        FetchError::DeltaExpired => format!(
            "Microsoft Calendar delta token expired ({account}) — the next sync restarts from scratch"
        ),
        FetchError::Other(m) => format!("outlook-calendar {account}: {m}"),
    }
}

/// One page of the calendarView delta: raw event items + continuation links.
struct DeltaPage {
    items: Vec<Value>,
    /// `@odata.nextLink` — more pages remain in this drain.
    next_link: Option<String>,
    /// `@odata.deltaLink` — drain complete; save as the cursor.
    delta_link: Option<String>,
}

/// Parse a calendarView delta response into a [`DeltaPage`]. PURE — tested
/// against fixture JSON without network.
fn parse_delta_page(v: &Value) -> DeltaPage {
    let items = v
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    DeltaPage {
        items,
        next_link: v
            .get("@odata.nextLink")
            .and_then(Value::as_str)
            .map(str::to_string),
        delta_link: v
            .get("@odata.deltaLink")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// A thin Graph client with an injectable base URL for testing.
struct GraphClient {
    base: String,
    token: String,
}

impl GraphClient {
    fn get_json(&self, url: &str) -> Result<Value, FetchError> {
        match ureq::get(url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HTTP_TIMEOUT)
            .call()
        {
            Ok(r) => r
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(410, _)) => Err(FetchError::DeltaExpired),
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

}

// ---------------------------------------------------------------------------
// Normalization: Graph event JSON → CalendarOccurrence

/// The row id for one event in the shared calendar store.
fn row_id(account_id: &str, event_id: &str) -> String {
    format!("{MICROSOFT_ROW_PREFIX}{account_id}/{event_id}")
}

/// The id prefix owning every row of one account — the snapshot scope.
fn account_prefix(account_id: &str) -> String {
    format!("{MICROSOFT_ROW_PREFIX}{account_id}/")
}

/// One Graph `dateTimeTimeZone` object → RFC3339 local.
fn parse_graph_dt(v: &Value) -> Option<String> {
    let dt_str = v.get("dateTime").and_then(Value::as_str)?;
    // _tz_str is read here for documentation; see comment below for why it is
    // always treated as UTC regardless of value.
    let _tz_str = v.get("timeZone").and_then(Value::as_str).unwrap_or("UTC");
    // Graph returns datetimes without explicit offset (e.g. "2017-04-21T10:00:00.0000000")
    // and a separate timeZone. We parse as naive then localize.
    let naive = chrono::NaiveDateTime::parse_from_str(
        dt_str.get(..19).unwrap_or(dt_str),
        "%Y-%m-%dT%H:%M:%S",
    )
    .ok()?;
    // Graph returns UTC by default (no Prefer: outlook.timezone header is
    // sent). Always interpret the naive datetime as UTC and convert to local.
    // If a non-UTC timeZone is ever returned the raw layer still has the
    // original value; treating as UTC is the safest fallback (avoids silently
    // placing wall-clock time in whatever zone the machine happens to be in).
    let local = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc)
        .with_timezone(&Local);
    Some(local.to_rfc3339())
}

/// RFC3339 local at a wall-clock time on a day.
fn local_at(d: NaiveDate, h: u32, m: u32, s: u32) -> Option<String> {
    Local
        .from_local_datetime(&d.and_hms_opt(h, m, s)?)
        .earliest()
        .map(|t| t.to_rfc3339())
}

/// One Graph event object → a [`CalendarOccurrence`], or `None` for events
/// missing the required fields or flagged `@removed` (deletions).
fn normalize_event(
    item: &Value,
    account_id: &str,
    account_email: &str,
) -> Option<NormalizedEvent> {
    // Deletions carry `@removed` — caller handles them separately.
    if item.get("@removed").is_some() {
        let id = item.get("id").and_then(Value::as_str)?;
        return Some(NormalizedEvent::Removed { row: row_id(account_id, id) });
    }

    let event_id = item.get("id").and_then(Value::as_str)?;

    let is_all_day = item.get("isAllDay").and_then(Value::as_bool).unwrap_or(false);
    let (start, end) = if is_all_day {
        // All-day events: start/end carry a bare date in dateTime with no
        // useful time component. Use midnight-to-midnight-local bounds.
        let s_raw = item
            .get("start")
            .and_then(|v| v.get("dateTime"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let e_raw = item
            .get("end")
            .and_then(|v| v.get("dateTime"))
            .and_then(Value::as_str)
            .unwrap_or(s_raw);
        let s_date = NaiveDate::parse_from_str(s_raw.get(..10).unwrap_or(""), "%Y-%m-%d").ok()?;
        // Graph all-day end is exclusive (same as Google). The vault wants
        // inclusive last-day-at-23:59:59.
        let e_date = NaiveDate::parse_from_str(e_raw.get(..10).unwrap_or(""), "%Y-%m-%d")
            .ok()
            .map(|d| d.pred_opt().unwrap_or(s_date).max(s_date))
            .unwrap_or(s_date);
        (local_at(s_date, 0, 0, 0)?, local_at(e_date, 23, 59, 59)?)
    } else {
        let s = parse_graph_dt(item.get("start")?.clone().as_object().map(|_| item.get("start").unwrap()).unwrap_or(&Value::Null))?;
        let e = item
            .get("end")
            .and_then(|v| parse_graph_dt(v))
            .unwrap_or_else(|| s.clone());
        (s, e)
    };

    let recurring = item.get("seriesMasterId").and_then(Value::as_str).is_some()
        || item
            .get("type")
            .and_then(Value::as_str)
            .map(|t| t == "seriesMaster" || t == "occurrence" || t == "exception")
            .unwrap_or(false);

    // For recurring occurrences, the originalStart is the stable slot key.
    // Graph `originalStart` is always UTC (e.g. "2016-12-25T06:00:00Z").
    // The contract specifies `occurrence` as RFC3339-local (matching the
    // EventKit and Google Calendar collectors), so parse as UTC + convert.
    let occurrence = if recurring {
        item.get("originalStart")
            .and_then(Value::as_str)
            .and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Local).to_rfc3339())
                    // Fallback: bare datetime without offset — treat as UTC.
                    .or_else(|| {
                        chrono::NaiveDateTime::parse_from_str(
                            s.get(..19).unwrap_or(s),
                            "%Y-%m-%dT%H:%M:%S",
                        )
                        .ok()
                        .map(|naive| {
                            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                                naive,
                                chrono::Utc,
                            )
                            .with_timezone(&Local)
                            .to_rfc3339()
                        })
                    })
            })
            .unwrap_or_else(|| start.clone())
    } else {
        // Non-recurring: empty slot so the key is stable across moves.
        String::new()
    };

    let text = |key: &str| -> String {
        item.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
    };

    let location = item
        .get("location")
        .and_then(|l| l.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Organizer is captured in the raw layer at full fidelity; it does not
    // belong in the `account` contract field (that's the syncing mailbox).
    let _organizer = item
        .get("organizer")
        .and_then(|o| o.get("emailAddress"))
        .and_then(|e| {
            e.get("name")
                .or_else(|| e.get("address"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string();

    let attendees: Vec<String> = item
        .get("attendees")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    a.get("emailAddress").and_then(|e| {
                        e.get("name")
                            .or_else(|| e.get("address"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // isCancelled comes in the event body (not via @removed); map it.
    let is_cancelled = item.get("isCancelled").and_then(Value::as_bool).unwrap_or(false);

    // showAs: "free" | "tentative" | "busy" | "oof" | "workingElsewhere"
    let status = if is_cancelled {
        "canceled".to_string()
    } else {
        match item.get("showAs").and_then(Value::as_str).unwrap_or("") {
            "tentative" => "tentative".to_string(),
            "busy" => "confirmed".to_string(),
            _ => String::new(),
        }
    };

    let notes = {
        // bodyPreview is plain text; body.content is HTML. Use bodyPreview.
        text("bodyPreview")
    };

    Some(NormalizedEvent::Upsert(CalendarOccurrence {
        id: row_id(account_id, event_id),
        occurrence,
        start,
        end,
        all_day: is_all_day,
        title: text("subject"),
        calendar: String::new(), // Graph calendarView doesn't return calendar name per event
        // `account` is the SYNCING mailbox (e.g. "dwills@example.com"), NOT
        // the event organizer. The organizer is already captured in the raw
        // layer; using String::max was incorrect (lexicographic ordering is not
        // a semantic preference and can place the organizer display name here).
        account: account_email.to_string(),
        location,
        notes,
        attendees,
        status,
        recurring,
    }))
}

/// What one normalized item is.
enum NormalizedEvent {
    Upsert(CalendarOccurrence),
    Removed { row: String },
}

// ---------------------------------------------------------------------------
// Sync logic

/// One full sync pass across every connected Microsoft account.
fn collect(vault: &Vault) -> Result<McalSyncStats> {
    let accounts = microsoft_accounts(vault)?;
    let mut state = vault.read_mcal_sync().unwrap_or_default();

    // Prune state for disconnected accounts.
    let live: std::collections::HashSet<&str> =
        accounts.iter().map(|a| a.id.as_str()).collect();
    state.accounts.retain(|id, _| live.contains(id.as_str()));

    if accounts.is_empty() {
        if vault.resolve(STATE_FILE).map(|p| p.exists()).unwrap_or(false) {
            state.updated = Local::now().to_rfc3339();
            vault.write_mcal_sync(&state)?;
            vault.write_mcal_index(&state)?;
        }
        return Ok(McalSyncStats::default());
    }

    let now = Local::now();
    let time_min = (now - ChronoDuration::days(WINDOW_PAST_DAYS)).to_rfc3339();
    let time_max = (now + ChronoDuration::days(WINDOW_FUTURE_DAYS)).to_rfc3339();
    let win_start = &time_min[..10];
    let win_end = &time_max[..10];
    let ts = now.to_rfc3339();

    let mut stats = McalSyncStats::default();

    for acct in &accounts {
        if acct.needs_reconnect {
            continue;
        }
        {
            let ast = state.accounts.entry(acct.id.clone()).or_default();
            ast.email = acct.email.clone();
            ast.error = None;
        }
        let token = match microsoft_fresh_token(vault, &acct.id) {
            Ok(t) => t,
            Err(e) => {
                state.accounts.entry(acct.id.clone()).or_default().error =
                    Some(format!("{e:#}"));
                continue;
            }
        };
        let client = GraphClient { base: GRAPH_BASE.to_string(), token };
        match sync_account(
            vault,
            &client,
            &acct.id,
            &acct.email,
            &mut state,
            win_start,
            win_end,
            &time_min,
            &time_max,
            &ts,
        ) {
            Ok(s) => {
                stats.accounts += 1;
                stats.baselined += s.baselined;
                stats.added += s.added;
                stats.changed += s.changed;
                stats.removed += s.removed;
            }
            Err(e) => {
                state.accounts.entry(acct.id.clone()).or_default().error =
                    Some(status_message(&acct.email, e));
            }
        }
    }

    state.updated = ts;
    vault.write_mcal_sync(&state)?;
    vault.write_mcal_index(&state)?;
    Ok(stats)
}

/// One account's sync: drain the delta feed (from cursor or a fresh window),
/// write raw + contract layers, advance the cursor only after a full drain.
/// `fetch` is injected so the orchestration can be tested without a network.
fn sync_account(
    vault: &Vault,
    client: &GraphClient,
    account_id: &str,
    account_email: &str,
    state: &mut McalSyncState,
    win_start: &str,
    win_end: &str,
    time_min: &str,
    time_max: &str,
    ts: &str,
) -> Result<McalAccountPassStats, FetchError> {
    let base = client.base.clone();
    sync_account_with_fetch(
        vault,
        account_id,
        account_email,
        state,
        win_start,
        win_end,
        time_min,
        time_max,
        ts,
        &base,
        &mut |url: &str| client.get_json(url),
    )
}

/// The actual drain logic with an injectable fetch function for testing.
/// `graph_base` is the Graph API base URL (injectable for tests).
fn sync_account_with_fetch(
    vault: &Vault,
    account_id: &str,
    account_email: &str,
    state: &mut McalSyncState,
    win_start: &str,
    win_end: &str,
    time_min: &str,
    time_max: &str,
    ts: &str,
    graph_base: &str,
    fetch: &mut dyn FnMut(&str) -> Result<Value, FetchError>,
) -> Result<McalAccountPassStats, FetchError> {
    let prefix = account_prefix(account_id);

    // Build the initial URL: stored deltaLink → just use it directly;
    // no cursor → fresh windowed request.
    let stored_cursor = state
        .accounts
        .get(account_id)
        .and_then(|a| a.delta_link.clone());
    let first_url = stored_cursor
        .clone()
        .unwrap_or_else(|| format!("{graph_base}/me/calendarView/delta?startDateTime={time_min}&endDateTime={time_max}"));
    let is_first_sync = stored_cursor.is_none()
        && !vault
            .calendar_range(win_start, win_end)
            .map(|r| r.iter().any(|o| o.id.starts_with(&prefix)))
            .unwrap_or(false);

    // Load the stored snapshot for this account (incremental: diff against it;
    // first run: empty working set, merge silently).
    let stored: Vec<CalendarOccurrence> = vault
        .calendar_range(win_start, win_end)
        .map_err(soft)?
        .into_iter()
        .filter(|o| o.id.starts_with(&prefix))
        .collect();
    let mut working: BTreeMap<String, CalendarOccurrence> = if is_first_sync {
        BTreeMap::new()
    } else {
        stored.iter().map(|o| (o.key(), o.clone())).collect()
    };

    // Drain all pages.
    let mut url = first_url;
    let mut raw_batch: Vec<Value> = Vec::new();
    let mut final_delta_link: Option<String> = None;
    let mut reset = false;

    loop {
        let page_val = match fetch(&url) {
            Ok(v) => v,
            Err(FetchError::DeltaExpired) if !reset => {
                // Cursor expired — restart with a fresh window.
                reset = true;
                working.clear();
                url = format!(
                    "{graph_base}/me/calendarView/delta?startDateTime={time_min}&endDateTime={time_max}"
                );
                continue;
            }
            Err(e) => return Err(e),
        };
        let page = parse_delta_page(&page_val);

        for item in &page.items {
            // Raw layer — append full fidelity.
            raw_batch.push(item.clone());
        }

        for item in &page.items {
            match normalize_event(item, account_id, account_email) {
                Some(NormalizedEvent::Upsert(occ)) => {
                    working.insert(occ.key(), occ);
                }
                Some(NormalizedEvent::Removed { row }) => {
                    working.retain(|_, o| o.id != row);
                }
                None => {}
            }
        }

        if let Some(dl) = page.delta_link {
            final_delta_link = Some(dl);
            break;
        }
        match page.next_link {
            Some(nl) => url = nl,
            None => break, // unexpected end, but don't loop forever
        }
    }

    // Write raw layer unconditionally (full fidelity).
    if !raw_batch.is_empty() {
        // We need a ts field to partition by. Attach the current ts to each
        // raw item so the stream can partition it. We wrap in a thin envelope
        // so the raw file stays machine-readable without altering the Graph
        // response shape.
        #[derive(Serialize)]
        struct RawEnvelope<'a> {
            ts: &'a str,
            account_id: &'a str,
            #[serde(flatten)]
            event: &'a Value,
        }
        let enveloped: Vec<RawEnvelope> = raw_batch
            .iter()
            .map(|e| RawEnvelope { ts, account_id, event: e })
            .collect();
        vault
            .stream(RAW_DIR, Partition::Month)
            .append(&enveloped, |e| e.ts)
            .map_err(soft)?;
    }

    // Clamp working set to the diff window.
    let mut fresh: Vec<CalendarOccurrence> = working
        .into_values()
        .filter(|o| o.day() >= win_start && o.day() <= win_end)
        .collect();
    fresh.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.key().cmp(&b.key())));

    // Write the contract layer: snapshot or baseline.
    let pass_stats = if is_first_sync {
        vault.calendar_backfill(&fresh).map_err(soft)?;
        McalAccountPassStats { baselined: fresh.len() as u64, ..Default::default() }
    } else {
        let s = vault
            .calendar_snapshot_scoped(&fresh, win_start, win_end, ts, &|o| {
                o.id.starts_with(&prefix)
            })
            .map_err(soft)?;
        // calendar_snapshot_scoped returns baseline=true when the events dir
        // is being created for the first time — treat those events as baselined
        // rather than "added" (the first-ever snapshot is state, not a change).
        let (baselined, added) = if s.baseline {
            (fresh.len() as u64, 0u64)
        } else {
            (0u64, s.added)
        };
        McalAccountPassStats { added, changed: s.changed, removed: s.removed, baselined }
    };

    // Advance the cursor ONLY after the full drain.
    let ast = state.accounts.entry(account_id.to_string()).or_default();
    ast.email = account_email.to_string();
    if let Some(dl) = final_delta_link {
        ast.delta_link = Some(dl);
    } else if reset {
        ast.delta_link = None; // cursor expired and we reset; will re-drain next pass
    }
    ast.events = fresh.len() as u64;
    vault.write_mcal_sync(state).map_err(soft)?;

    Ok(pass_stats)
}

/// Per-account pass stats (rolled into [`McalSyncStats`]).
#[derive(Default)]
struct McalAccountPassStats {
    baselined: u64,
    added: u64,
    changed: u64,
    removed: u64,
}

impl Vault {
    /// The persisted Outlook Calendar sync state, if a sync has ever run.
    pub fn read_mcal_sync(&self) -> Option<McalSyncState> {
        let path = self.resolve(STATE_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_mcal_sync(&self, state: &McalSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(STATE_FILE)?, state)
    }

    fn write_mcal_index(&self, state: &McalSyncState) -> Result<()> {
        let mut md = format!(
            "# Outlook Calendar\n\nLast sync: {}\n\n\
             | Account | Events | Error |\n|---|---|---|\n",
            state.updated
        );
        for a in state.accounts.values() {
            let err = a.error.as_deref().unwrap_or("");
            md.push_str(&format!("| {} | {} | {} |\n", a.email, a.events, err));
        }
        crate::store::write_atomic(&self.resolve(INDEX_FILE)?, md.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Registry face

fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    // Silent no-op when no Microsoft account is connected.
    if microsoft_accounts(vault).map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    match collect(vault) {
        Ok(s) => Ok(crate::registry::CollectOutcome::note_if(
            s.baselined + s.added + s.changed + s.removed > 0,
            || {
                if s.baselined > 0 {
                    format!("outlook calendar baseline — {} events", s.baselined)
                } else {
                    format!(
                        "outlook calendar synced — {} added, {} changed, {} removed",
                        s.added, s.changed, s.removed
                    )
                }
            },
        )),
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "outlook calendar sync error: {e:#}"
        ))),
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_mcal_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    if microsoft_accounts(vault)?.is_empty() {
        bail!("no Microsoft account is connected");
    }
    let s = collect(vault)?;
    let errors = vault
        .read_mcal_sync()
        .map(|st| st.accounts.values().filter(|a| a.error.is_some()).count() as u64)
        .unwrap_or(0);
    let mut headline = if s.baselined > 0 {
        format!("{} events baselined across {} accounts", s.baselined, s.accounts)
    } else if s.added + s.changed + s.removed > 0 {
        format!(
            "{} added, {} changed, {} removed",
            s.added, s.changed, s.removed
        )
    } else {
        "calendar up to date".to_string()
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
            ("baselined", s.baselined),
            ("added", s.added),
            ("changed", s.changed),
            ("removed", s.removed),
            ("account_errors", errors),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "outlook-calendar",
        name: "Outlook Calendar",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs Microsoft Outlook calendar events into the unified calendar store \
                      via the Graph calendarView delta API. Shares the Microsoft login with \
                      Outlook email — one connection, two pulls.",
        domain: "calendar",
        vault_path: "calendar/",
        toggleable: true,
        setup: &[],
        caveats: "Exchange Web Services (EWS) is being retired by Microsoft in October 2026; \
                 this integration targets the Graph API only.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(MCAL_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("microsoft"),
    pull: Some(pull),
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-mcal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A Graph event JSON fixture (normal timed event).
    fn timed_event(id: &str, subject: &str, start_dt: &str, end_dt: &str) -> Value {
        json!({
            "id": id,
            "subject": subject,
            "start": { "dateTime": start_dt, "timeZone": "UTC" },
            "end": { "dateTime": end_dt, "timeZone": "UTC" },
            "isAllDay": false,
            "isCancelled": false,
            "type": "singleInstance",
            "showAs": "busy",
            "attendees": [],
            "organizer": {
                "emailAddress": { "name": "Alice", "address": "alice@contoso.com" }
            },
            "location": { "displayName": "Conference Room A" },
            "bodyPreview": "Sync meeting"
        })
    }

    /// A Graph all-day event fixture.
    fn all_day_event(id: &str, subject: &str, start_date: &str, end_date: &str) -> Value {
        json!({
            "id": id,
            "subject": subject,
            "start": { "dateTime": format!("{start_date}T00:00:00.0000000"), "timeZone": "UTC" },
            "end": { "dateTime": format!("{end_date}T00:00:00.0000000"), "timeZone": "UTC" },
            "isAllDay": true,
            "isCancelled": false,
            "type": "singleInstance",
            "showAs": "free",
            "attendees": []
        })
    }

    /// A deleted event item (carries `@removed`).
    fn removed_event(id: &str) -> Value {
        json!({
            "id": id,
            "@removed": { "reason": "deleted" },
            "@odata.type": "#microsoft.graph.event"
        })
    }

    // -- parse_delta_page --

    #[test]
    fn parse_delta_page_extracts_next_and_delta_links() {
        let v = json!({
            "value": [{"id": "A"}],
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$skiptoken=X"
        });
        let p = parse_delta_page(&v);
        assert_eq!(p.items.len(), 1);
        assert!(p.next_link.is_some());
        assert!(p.delta_link.is_none());

        let v2 = json!({
            "value": [],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=Y"
        });
        let p2 = parse_delta_page(&v2);
        assert!(p2.delta_link.is_some());
        assert!(p2.next_link.is_none());
    }

    // -- normalize_event --

    #[test]
    fn normalize_timed_event_fields() {
        let item = timed_event("ev1", "Standup", "2026-06-15T10:00:00.0000000", "2026-06-15T11:00:00.0000000");
        match normalize_event(&item, "acc1", "me@contoso.com").unwrap() {
            NormalizedEvent::Upsert(occ) => {
                assert_eq!(occ.id, "mcal:acc1/ev1");
                assert_eq!(occ.title, "Standup");
                assert!(!occ.all_day);
                assert_eq!(occ.location, "Conference Room A");
                assert_eq!(occ.notes, "Sync meeting");
                assert!(!occ.recurring);
                assert!(occ.occurrence.is_empty(), "non-recurring slot is empty");
                assert_eq!(occ.status, "confirmed");
            }
            NormalizedEvent::Removed { .. } => panic!("expected upsert"),
        }
    }

    #[test]
    fn normalize_all_day_event_inclusive_end() {
        // Graph all-day end date is exclusive: 2026-07-04 → last inclusive day is 2026-07-03.
        let item = all_day_event("ev2", "Holiday", "2026-07-01", "2026-07-04");
        match normalize_event(&item, "acc1", "me@contoso.com").unwrap() {
            NormalizedEvent::Upsert(occ) => {
                assert!(occ.all_day);
                assert!(occ.start.starts_with("2026-07-01T00:00:00"), "start={}", occ.start);
                assert!(occ.end.starts_with("2026-07-03T23:59:59"), "end={}", occ.end);
            }
            NormalizedEvent::Removed { .. } => panic!("expected upsert"),
        }
    }

    #[test]
    fn normalize_removed_item_yields_removed_variant() {
        let item = removed_event("ev3");
        match normalize_event(&item, "acc1", "me@contoso.com").unwrap() {
            NormalizedEvent::Removed { row } => assert_eq!(row, "mcal:acc1/ev3"),
            NormalizedEvent::Upsert(_) => panic!("expected removed"),
        }
    }

    #[test]
    fn normalize_cancelled_event_maps_status() {
        let mut item = timed_event("ev4", "Cancelled meeting", "2026-06-20T14:00:00.0000000", "2026-06-20T15:00:00.0000000");
        item["isCancelled"] = json!(true);
        match normalize_event(&item, "acc1", "me@contoso.com").unwrap() {
            NormalizedEvent::Upsert(occ) => assert_eq!(occ.status, "canceled"),
            NormalizedEvent::Removed { .. } => panic!("expected upsert"),
        }
    }

    #[test]
    fn normalize_recurring_event_sets_occurrence_slot() {
        let mut item = timed_event("ev5_inst", "Daily standup", "2026-06-16T09:00:00.0000000", "2026-06-16T09:30:00.0000000");
        item["type"] = json!("occurrence");
        item["seriesMasterId"] = json!("ev5");
        item["originalStart"] = json!("2026-06-16T09:00:00.0000000");
        match normalize_event(&item, "acc1", "me@contoso.com").unwrap() {
            NormalizedEvent::Upsert(occ) => {
                assert!(occ.recurring);
                assert!(!occ.occurrence.is_empty(), "recurring slot must be non-empty");
            }
            NormalizedEvent::Removed { .. } => panic!("expected upsert"),
        }
    }

    // -- row_id / account_prefix --

    #[test]
    fn row_id_and_prefix_are_consistent() {
        let rid = row_id("ACC123", "EVTABC");
        assert!(rid.starts_with(MICROSOFT_ROW_PREFIX));
        assert!(rid.starts_with(&account_prefix("ACC123")));
        assert_eq!(rid, "mcal:ACC123/EVTABC");
    }

    // -- state file round-trip --

    #[test]
    fn mcal_sync_state_roundtrip() {
        let v = temp_vault("state-rt");
        let state = McalSyncState {
            updated: "2026-06-15T12:00:00+00:00".to_string(),
            accounts: {
                let mut m = BTreeMap::new();
                m.insert("acc1".to_string(), McalAccountState {
                    email: "me@outlook.com".to_string(),
                    delta_link: Some("https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=XYZ".to_string()),
                    events: 5,
                    error: None,
                });
                m
            },
        };
        v.write_mcal_sync(&state).unwrap();
        let loaded = v.read_mcal_sync().unwrap();
        assert_eq!(loaded.updated, state.updated);
        let a = loaded.accounts.get("acc1").unwrap();
        assert_eq!(a.email, "me@outlook.com");
        assert_eq!(a.events, 5);
        assert!(a.delta_link.is_some());
    }

    // -- sync_account_with_fetch (full orchestration) --

    #[test]
    fn first_sync_baselines_events_silently() {
        let v = temp_vault("first-sync");
        let mut state = McalSyncState::default();
        let ts = Local::now().to_rfc3339();
        let now = Local::now();
        let win_start = (now - ChronoDuration::days(WINDOW_PAST_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let win_end = (now + ChronoDuration::days(WINDOW_FUTURE_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        // Two events in the window.
        let e1_start = format!("{}T10:00:00.0000000", &ts[..10]);
        let e1_end = format!("{}T11:00:00.0000000", &ts[..10]);
        let e2_start = format!("{}T14:00:00.0000000", &ts[..10]);
        let e2_end = format!("{}T15:00:00.0000000", &ts[..10]);
        let page1 = json!({
            "value": [
                timed_event("E1", "Standup", &e1_start, &e1_end),
                timed_event("E2", "1:1", &e2_start, &e2_end),
            ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=FIRST"
        });
        let mut calls = 0usize;
        let mut fetch = |_url: &str| -> Result<Value, FetchError> {
            calls += 1;
            Ok(page1.clone())
        };
        let stats = sync_account_with_fetch(
            &v, "ACC1", "me@contoso.com", &mut state,
            &win_start, &win_end,
            &format!("{}T00:00:00Z", &win_start),
            &format!("{}T23:59:59Z", &win_end),
            &ts, GRAPH_BASE, &mut fetch,
        ).unwrap();
        assert_eq!(calls, 1);
        assert_eq!(stats.baselined, 2, "first sync baselines both events");
        assert_eq!(stats.added, 0, "no change lines on baseline");
        // Cursor was advanced.
        assert!(state.accounts.get("ACC1").unwrap().delta_link.is_some());
    }

    #[test]
    fn incremental_sync_detects_added_events() {
        let v = temp_vault("incr-sync");
        let now = Local::now();
        let ts = now.to_rfc3339();
        let win_start = (now - ChronoDuration::days(WINDOW_PAST_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let win_end = (now + ChronoDuration::days(WINDOW_FUTURE_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let e_start = format!("{}T10:00:00.0000000", &ts[..10]);
        let e_end = format!("{}T11:00:00.0000000", &ts[..10]);

        // First sync — baseline one event.
        let mut state = McalSyncState::default();
        let page_first = json!({
            "value": [ timed_event("E1", "First", &e_start, &e_end) ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=T1"
        });
        let _ = sync_account_with_fetch(
            &v, "ACC1", "me@contoso.com", &mut state,
            &win_start, &win_end,
            &format!("{}T00:00:00Z", &win_start),
            &format!("{}T23:59:59Z", &win_end),
            &ts, GRAPH_BASE, &mut |_| Ok(page_first.clone()),
        ).unwrap();

        // Second sync — new event appears.
        let e2_start = format!("{}T13:00:00.0000000", &ts[..10]);
        let e2_end = format!("{}T14:00:00.0000000", &ts[..10]);
        let page_second = json!({
            "value": [ timed_event("E2", "New", &e2_start, &e2_end) ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=T2"
        });
        let stats2 = sync_account_with_fetch(
            &v, "ACC1", "me@contoso.com", &mut state,
            &win_start, &win_end,
            &format!("{}T00:00:00Z", &win_start),
            &format!("{}T23:59:59Z", &win_end),
            &ts, GRAPH_BASE, &mut |_| Ok(page_second.clone()),
        ).unwrap();
        assert_eq!(stats2.added, 1, "one new event on incremental sync");
        assert_eq!(stats2.baselined, 0);
    }

    #[test]
    fn removal_drops_event_from_snapshot() {
        let v = temp_vault("removal");
        let now = Local::now();
        let ts = now.to_rfc3339();
        let win_start = (now - ChronoDuration::days(WINDOW_PAST_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let win_end = (now + ChronoDuration::days(WINDOW_FUTURE_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let e_start = format!("{}T10:00:00.0000000", &ts[..10]);
        let e_end = format!("{}T11:00:00.0000000", &ts[..10]);

        // First sync — baseline one event.
        let mut state = McalSyncState::default();
        let page_first = json!({
            "value": [ timed_event("E1", "Meeting", &e_start, &e_end) ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=T1"
        });
        let _ = sync_account_with_fetch(
            &v, "ACC1", "me@contoso.com", &mut state,
            &win_start, &win_end,
            &format!("{}T00:00:00Z", &win_start),
            &format!("{}T23:59:59Z", &win_end),
            &ts, GRAPH_BASE, &mut |_| Ok(page_first.clone()),
        ).unwrap();

        // Second sync — event E1 deleted.
        let page_remove = json!({
            "value": [ removed_event("E1") ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=T2"
        });
        let stats2 = sync_account_with_fetch(
            &v, "ACC1", "me@contoso.com", &mut state,
            &win_start, &win_end,
            &format!("{}T00:00:00Z", &win_start),
            &format!("{}T23:59:59Z", &win_end),
            &ts, GRAPH_BASE, &mut |_| Ok(page_remove.clone()),
        ).unwrap();
        assert_eq!(stats2.removed, 1, "one event removed");
    }

    #[test]
    fn delta_expired_resets_and_redrains() {
        let v = temp_vault("delta-expired");
        let now = Local::now();
        let ts = now.to_rfc3339();
        let win_start = (now - ChronoDuration::days(WINDOW_PAST_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let win_end = (now + ChronoDuration::days(WINDOW_FUTURE_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let e_start = format!("{}T10:00:00.0000000", &ts[..10]);
        let e_end = format!("{}T11:00:00.0000000", &ts[..10]);

        // Seed a stale cursor.
        let mut state = McalSyncState::default();
        state.accounts.insert("ACC1".to_string(), McalAccountState {
            email: "me@contoso.com".to_string(),
            delta_link: Some("https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=STALE".to_string()),
            events: 1,
            error: None,
        });

        let fresh_page = json!({
            "value": [ timed_event("E1", "Reset event", &e_start, &e_end) ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=NEW"
        });
        let mut call_count = 0usize;
        let stats = sync_account_with_fetch(
            &v, "ACC1", "me@contoso.com", &mut state,
            &win_start, &win_end,
            &format!("{}T00:00:00Z", &win_start),
            &format!("{}T23:59:59Z", &win_end),
            &ts, GRAPH_BASE,
            &mut |url: &str| {
                call_count += 1;
                if url.contains("STALE") {
                    Err(FetchError::DeltaExpired)
                } else {
                    Ok(fresh_page.clone())
                }
            },
        ).unwrap();
        assert_eq!(call_count, 2, "first call expired, second re-drain");
        assert!(
            state.accounts.get("ACC1").unwrap().delta_link.as_deref().unwrap_or("").contains("NEW"),
            "new cursor saved after reset"
        );
        // After a delta reset we had a stored cursor so is_first_sync=false;
        // the event appears as "added" (snapshot diff), not "baselined".
        assert_eq!(stats.added + stats.baselined, 1, "one event after reset drain");
    }

    #[test]
    fn raw_layer_written_on_sync() {
        let v = temp_vault("raw-layer");
        let now = Local::now();
        let ts = now.to_rfc3339();
        let win_start = (now - ChronoDuration::days(WINDOW_PAST_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let win_end = (now + ChronoDuration::days(WINDOW_FUTURE_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let e_start = format!("{}T10:00:00.0000000", &ts[..10]);
        let e_end = format!("{}T11:00:00.0000000", &ts[..10]);

        let mut state = McalSyncState::default();
        let page = json!({
            "value": [ timed_event("EX1", "Raw Test", &e_start, &e_end) ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=RAW1"
        });
        sync_account_with_fetch(
            &v, "ACC1", "me@contoso.com", &mut state,
            &win_start, &win_end,
            &format!("{}T00:00:00Z", &win_start),
            &format!("{}T23:59:59Z", &win_end),
            &ts, GRAPH_BASE, &mut |_| Ok(page.clone()),
        ).unwrap();

        // At least one raw partition file should exist.
        let raw_path = v.resolve(RAW_DIR).unwrap();
        assert!(raw_path.exists(), "raw dir should be created");
        let files: Vec<_> = fs::read_dir(&raw_path).unwrap().collect();
        assert!(!files.is_empty(), "at least one raw partition file");
    }

    // -- regression: defect fixes --

    /// Defect 1 (major): `account` must always be the SYNCING mailbox,
    /// never the event organizer or a lexicographic winner between them.
    #[test]
    fn account_field_is_syncing_mailbox_not_organizer() {
        // Case A: organizer name lexicographically GREATER than mailbox
        // ('Z' > 'a') — the old String::max would have returned the organizer.
        let mut item = timed_event(
            "ev_acc_a",
            "Zoom call",
            "2026-06-15T10:00:00.0000000",
            "2026-06-15T11:00:00.0000000",
        );
        item["organizer"] = json!({
            "emailAddress": { "name": "Zoom Meeting", "address": "zoom@zoom.us" }
        });
        let mailbox = "alice@contoso.com";
        match normalize_event(&item, "acc1", mailbox).unwrap() {
            NormalizedEvent::Upsert(occ) => {
                assert_eq!(
                    occ.account, mailbox,
                    "account must be the syncing mailbox even when organizer sorts higher"
                );
                assert_ne!(
                    occ.account, "Zoom Meeting",
                    "organizer display name must NOT appear in account field"
                );
            }
            NormalizedEvent::Removed { .. } => panic!("expected upsert"),
        }

        // Case B: organizer name lexicographically LESS than mailbox —
        // the old String::max would have returned the mailbox by accident.
        let mut item2 = timed_event(
            "ev_acc_b",
            "Internal sync",
            "2026-06-15T10:00:00.0000000",
            "2026-06-15T11:00:00.0000000",
        );
        item2["organizer"] = json!({
            "emailAddress": { "name": "Samantha Booth", "address": "samanthab@contoso.com" }
        });
        let mailbox2 = "zwills@example.com"; // 'z' > 's', old code returns "zwills..." (correct by accident)
        match normalize_event(&item2, "acc2", mailbox2).unwrap() {
            NormalizedEvent::Upsert(occ) => {
                assert_eq!(occ.account, mailbox2, "account is always the syncing mailbox");
                assert_ne!(occ.account, "Samantha Booth", "organizer must not leak into account");
            }
            NormalizedEvent::Removed { .. } => panic!("expected upsert"),
        }
    }

    /// Defect 2 (minor): `occurrence` slot for recurring events must be
    /// RFC3339-local, not a raw UTC `Z`-suffixed string.
    /// Graph originalStart is always UTC ("2016-12-25T06:00:00Z"); after
    /// conversion the slot must not end in `Z` and must contain an offset.
    #[test]
    fn recurring_occurrence_slot_is_rfc3339_local() {
        let mut item = timed_event(
            "ev_rec",
            "Daily standup",
            "2026-06-16T09:00:00.0000000",
            "2026-06-16T09:30:00.0000000",
        );
        item["type"] = json!("occurrence");
        item["seriesMasterId"] = json!("ev_rec_master");
        // Mimic Graph's UTC originalStart (always ends in "Z").
        item["originalStart"] = json!("2026-06-16T09:00:00Z");

        match normalize_event(&item, "acc1", "me@contoso.com").unwrap() {
            NormalizedEvent::Upsert(occ) => {
                assert!(occ.recurring);
                assert!(!occ.occurrence.is_empty(), "recurring slot must be non-empty");
                // After UTC→local conversion the result must NOT end with "Z"
                // (it should carry a fixed offset like "+00:00" or "-07:00").
                assert!(
                    !occ.occurrence.ends_with('Z'),
                    "occurrence slot must be RFC3339-local (not UTC Z-suffix), got: {}",
                    occ.occurrence
                );
                // Must contain an offset marker ('+' or '-') after the time component.
                assert!(
                    occ.occurrence.len() > 19
                        && (occ.occurrence.contains('+') || occ.occurrence[19..].contains('-')),
                    "occurrence must carry an explicit UTC offset, got: {}",
                    occ.occurrence
                );
            }
            NormalizedEvent::Removed { .. } => panic!("expected upsert"),
        }
    }

    /// Defect 3 (minor): parse_graph_dt must not silently place a non-UTC
    /// wall-clock datetime in the machine's local zone. Since we now always
    /// treat the naive datetime as UTC, a Windows timezone label in the
    /// timeZone field must NOT cause a different result than "UTC".
    #[test]
    fn parse_graph_dt_non_utc_timezone_treated_as_utc() {
        // Same datetime, one claims UTC, one claims a Windows TZ name.
        let v_utc = json!({ "dateTime": "2026-06-15T10:00:00.0000000", "timeZone": "UTC" });
        let v_win = json!({ "dateTime": "2026-06-15T10:00:00.0000000", "timeZone": "Pacific Standard Time" });
        let result_utc = parse_graph_dt(&v_utc).expect("UTC parse must succeed");
        let result_win = parse_graph_dt(&v_win).expect("Windows TZ parse must succeed");
        assert_eq!(
            result_utc, result_win,
            "non-UTC timeZone label must not change the converted local time \
             (both treated as UTC until full Windows-TZ mapping is implemented)"
        );
    }
}
