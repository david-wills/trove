//! Calendly — scheduled meeting history via the v2 REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/calendly.md.
//!
//! Two vault destinations written per pull:
//!
//! - **Raw:** `calendar/calendly/raw/YYYY-MM.jsonl` — verbatim event +
//!   invitee objects from the API, full fidelity, partitioned by event
//!   `start_time` month, deduped by event URI. The raw envelope preserves
//!   full-fidelity data (event-type URI, location detail, questions/answers,
//!   cancellation details) verbatim.
//! - **Contract:** `calendar/calendly/YYYY-MM.jsonl` — one
//!   [`crate::calendar::CalendarOccurrence`] row per booked event, partitioned
//!   by `start_time` month. Status mutations (active→canceled) are upserted:
//!   if an event's status changed since the last pull the stored row is
//!   rewritten with the new status. Invitee names/emails land in `attendees`.
//!
//! ## API — Calendly v2 (`https://api.calendly.com`)
//!
//! Auth: `Authorization: Bearer <PAT>`. Personal access tokens are issued at
//! Calendly Settings → Integrations → API & Webhooks — no app registration
//! needed. Rate limit: 100 req/min (well within a personal hourly pull).
//!
//! Steps per pull:
//! 1. `GET /users/me` → get the user URI (needed for the events query).
//! 2. `GET /scheduled_events?user=<uri>&status=active,canceled&count=100`
//!    Drain all pages via `pagination.next_page_token`.
//! 3. For each new event: `GET /scheduled_events/{uuid}/invitees?count=100`
//!    to fetch attendee detail.
//!
//! Response shapes (v2):
//! - List: `{ "collection": [...], "pagination": { "count": N, "next_page":
//!   "url", "next_page_token": "token" | null } }`
//! - Event object: `uri`, `name` (event-type name), `start_time`, `end_time`,
//!   `status` (`"active"` | `"canceled"`), `event_type` (uri), `location`
//!   (`{ "type": "...", "location": "...", "join_url": "..." }`),
//!   `cancellation` (object, when canceled)
//! - Invitee: `uri`, `email`, `name`, `status`, `timezone`,
//!   `questions_and_answers` ([`{ "question", "answer" }`])
//!
//! ## Cursor
//!
//! `.trove/calendly-sync.json` holds `initialized` — a flag that is `true`
//! after the first successful pull. The query window is always a rolling
//! `[now − LOOKBACK_DAYS, now + LOOKAHEAD_DAYS]` regardless of any prior
//! pulls. This avoids the "lookahead poison" problem: if a future event's
//! `start_time` were used as the next `min_start_time`, every booking made
//! between now and that future event would fall below the floor and be silently
//! lost. The rolling window also ensures status mutations (cancellations) are
//! detected on re-fetch. New events are appended; events whose status changed
//! since last pull are upserted (the stored row is rewritten).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::calendar::CalendarOccurrence;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "calendly";
const CONTRACT_DIR: &str = "calendar/calendly";
const RAW_DIR: &str = "calendar/calendly/raw";
const SYNC_FILE: &str = ".trove/calendly-sync.json";
const SERVICE: &str = "calendly";

const API_BASE: &str = "https://api.calendly.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE_SIZE: u64 = 100;
/// Hourly — Calendly scheduling history doesn't need sub-hourly freshness.
const SYNC_SECS: u64 = 3600;
/// How far back to look on every pull (rolling window, not cursor-driven).
/// We always scan the last LOOKBACK_DAYS to capture status changes (cancellations).
const LOOKBACK_DAYS: i64 = 365;
/// How far forward to look (upcoming bookings are useful too).
const LOOKAHEAD_DAYS: i64 = 365;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "calendly synced — {} events ({} active, {} cancelled)",
                    c("events"),
                    c("active"),
                    c("cancelled"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "calendly sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Calendly synced — {} events ({} active, {} cancelled)",
            c("events"),
            c("active"),
            c("cancelled"),
        ),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "calendly",
        name: "Calendly",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Calendly scheduled-event history — including cancelled meetings \
                      and per-event invitee details — using a personal access token with no app \
                      registration required.",
        domain: "calendar",
        vault_path: "calendar/calendly/",
        toggleable: true,
        setup: &[
            "In Calendly, open Settings → Integrations → API & Webhooks.",
            "Click 'Generate New Token', give it a name, and copy the token.",
            "Paste it here — it's stored locally and never leaves your machine.",
        ],
        caveats: "Uses the Calendly v2 API with a personal access token — no OAuth app setup \
                  required. Pulls all scheduled events (active and cancelled) plus per-event \
                  invitee names and emails. Rate-limited to 100 req/min, well within a \
                  personal hourly sync budget.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some(SOURCE),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = Calendly personal access token, a SECRET).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!(
            "empty token — paste your Calendly personal access token from \
             Settings → Integrations → API & Webhooks"
        );
    }
    let client = CalendlyClient::new(API_BASE.to_string(), token.to_string());
    // Verify with a real call; surfaces a clear message on 401.
    match client.get("/users/me") {
        Ok(_) => {}
        Err(CalendlyError::Unauthorized) => bail!(
            "Calendly rejected the token (401) — check it's your personal access token \
             from Settings → Integrations → API & Webhooks and hasn't been revoked"
        ),
        Err(e) => bail!("Calendly /users/me check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
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
            label: "Calendly".to_string(),
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
    id: SOURCE,
    display_name: "Calendly",
    methods: &[ConnectMethod::TokenPaste {
        label: "Calendly personal access token",
        help: "Paste your Calendly personal access token from Settings → Integrations → API & Webhooks.",
        placeholder: "eyJra…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &[SOURCE],
    setup: &[
        "In Calendly, open Settings → Integrations → API & Webhooks.",
        "Click 'Generate New Token', give it a name (e.g. 'Trove'), and copy the token.",
        "Paste it here — it's stored locally and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP client — injectable trait so tests run fully offline.

#[derive(Debug)]
enum CalendlyError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for CalendlyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalendlyError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            CalendlyError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            CalendlyError::Other(m) => write!(f, "{m}"),
        }
    }
}

trait CalendlyApi {
    /// `GET <path>` (API-relative, e.g. `/users/me`). Returns the parsed JSON
    /// body. Path may include query parameters already.
    fn get(&self, path: &str) -> Result<Value, CalendlyError>;
}

struct CalendlyClient {
    base: String,
    token: String,
}

impl CalendlyClient {
    fn new(base: String, token: String) -> Self {
        CalendlyClient { base, token }
    }
}

impl CalendlyApi for CalendlyClient {
    fn get(&self, path: &str) -> Result<Value, CalendlyError> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .call();
        match resp {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| CalendlyError::Other(format!("parse response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(CalendlyError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(CalendlyError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(CalendlyError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(e) => CalendlyError::Other(e.to_string()).pipe_err(),
        }
    }
}

// Helper trait to avoid repetition; converts self into an Err.
trait PipeErr {
    fn pipe_err<T>(self) -> Result<T, CalendlyError>;
}
impl PipeErr for CalendlyError {
    fn pipe_err<T>(self) -> Result<T, CalendlyError> {
        Err(self)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// True after the first successful pull. The query window is always a
    /// rolling [now-LOOKBACK, now+LOOKAHEAD] — we do NOT store or advance a
    /// start-time cursor because advancing min_start_time past 'now' would
    /// strand near-term bookings beneath future-dated events (lookahead poison).
    #[serde(default)]
    initialized: bool,
    /// Wall-clock time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_calendly_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_calendly_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Drain helpers — all pages via next_page_token.

/// Drain all pages of a Calendly list endpoint into a flat Vec of JSON
/// objects. The collection wrapper key is `"collection"` and pagination lives
/// in `"pagination"` → `"next_page_token"`.
fn drain_pages(
    api: &impl CalendlyApi,
    first_path: &str,
) -> Result<Vec<Value>, CalendlyError> {
    let mut out = Vec::new();
    let mut path = first_path.to_string();
    loop {
        let body = api.get(&path)?;
        let items = body
            .get("collection")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        out.extend(items);
        let next = body
            .pointer("/pagination/next_page_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match next {
            Some(token) => {
                // Calendly provides the full next_page URL in
                // pagination.next_page as well; build from token instead so
                // the injected base URL survives in tests.
                let sep = if first_path.contains('?') { '&' } else { '?' };
                path = format!("{first_path}{sep}page_token={token}");
            }
            None => break,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Normalization: Calendly event JSON → CalendarOccurrence.

/// Extract the UUID from a Calendly resource URI like
/// `https://api.calendly.com/scheduled_events/<uuid>`.
fn uuid_from_uri(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

/// An RFC3339 timestamp (UTC or offset) → RFC3339 local.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Build a [`CalendarOccurrence`] from a raw Calendly event object and its
/// invitee list. Returns `None` if `start_time` is missing (can't partition).
fn occ_from_event(event: &Value, invitees: &[Value]) -> Option<CalendarOccurrence> {
    let uri = event.get("uri").and_then(Value::as_str).unwrap_or("");
    if uri.is_empty() {
        return None;
    }
    let uuid = uuid_from_uri(uri);
    let id = format!("cly:{uuid}");

    let start_raw = event.get("start_time").and_then(Value::as_str)?;
    let end_raw = event.get("end_time").and_then(Value::as_str).unwrap_or(start_raw);
    let start = to_local(start_raw);
    let end = to_local(end_raw);

    let status = event.get("status").and_then(Value::as_str).unwrap_or("active");
    // CalendarOccurrence uses "canceled" (single l — matching the shared
    // EventKit/Google Calendar convention) even though Calendly spells it
    // "cancelled" in its `status` field.
    let status_out = match status {
        "canceled" | "cancelled" => "canceled",
        _ => "confirmed",
    }
    .to_string();

    let title = event
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Prefer the human-readable location field; fall back to join_url (video
    // calls). Both may be present; skip empty strings.
    let location = event
        .pointer("/location/location")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            event
                .pointer("/location/join_url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string();

    // Attendees: the event owner + invitees. We skip the host (Calendly
    // doesn't include the host's name directly) and use invitee names
    // falling back to emails.
    let attendees: Vec<String> = invitees
        .iter()
        .filter_map(|inv| {
            inv.get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| inv.get("email").and_then(Value::as_str))
                .map(str::to_string)
        })
        .collect();

    Some(CalendarOccurrence {
        id,
        occurrence: String::new(), // non-recurring: no slot concept
        start,
        end,
        all_day: false, // Calendly events are always time-bound
        title,
        calendar: "Calendly".to_string(),
        account: String::new(), // the PAT owner; not exposed in /users/me
        location,
        notes: String::new(),
        attendees,
        status: status_out,
        recurring: false,
    })
}

// ---------------------------------------------------------------------------
// Raw envelope: event + its invitees in one line.

#[derive(Debug, Serialize, Deserialize)]
struct RawEvent {
    /// The event URI (stable dedup key).
    uri: String,
    /// The raw event JSON object.
    event: Map<String, Value>,
    /// Invitee objects for this event.
    invitees: Vec<Map<String, Value>>,
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Load all existing contract rows keyed by their `id` field, grouped by
/// partition key (YYYY-MM). Used to detect new vs. status-changed events.
fn existing_by_partition(
    vault: &Vault,
) -> Result<BTreeMap<String, Vec<CalendarOccurrence>>> {
    let stream = vault.stream(CONTRACT_DIR, Partition::Month);
    let mut out: BTreeMap<String, Vec<CalendarOccurrence>> = BTreeMap::new();
    for key in stream.partitions().unwrap_or_default() {
        let rows = stream.read::<CalendarOccurrence>(&key).unwrap_or_default();
        if !rows.is_empty() {
            out.insert(key, rows);
        }
    }
    Ok(out)
}

/// Write new + upserted rows (contract + raw).
///
/// - **New events**: appended to the contract and raw streams.
/// - **Status-changed events**: the affected contract partition is atomically
///   rewritten with the updated row in place. (Raw is append-only; the raw
///   envelope is re-appended so the latest state is always the last line.)
///
/// Returns `(total, active, cancelled)` across ALL events processed this pass
/// (new + updated).
fn write_layers(
    vault: &Vault,
    rows: Vec<(CalendarOccurrence, RawEvent)>,
    existing: &BTreeMap<String, Vec<CalendarOccurrence>>,
) -> Result<(u64, u64, u64)> {
    use std::collections::HashMap;

    let contract_stream = vault.stream(CONTRACT_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    // Bucket incoming rows by partition key.
    let mut by_key: BTreeMap<String, Vec<(CalendarOccurrence, RawEvent)>> = BTreeMap::new();
    for row in rows {
        let key = Partition::Month
            .key(row.0.start.as_str())
            .unwrap_or("")
            .to_string();
        by_key.entry(key).or_default().push(row);
    }

    let mut active = 0u64;
    let mut cancelled = 0u64;

    for (key, incoming) in &by_key {
        // Index existing rows for this partition by id.
        let existing_rows = existing.get(key).cloned().unwrap_or_default();
        let existing_by_id: HashMap<&str, &CalendarOccurrence> = existing_rows
            .iter()
            .map(|r| (r.id.as_str(), r))
            .collect();

        let mut truly_new: Vec<&CalendarOccurrence> = Vec::new();
        let mut new_raws: Vec<&RawEvent> = Vec::new();
        let mut needs_rewrite = false;

        for (occ, raw_ev) in incoming {
            if occ.status == "canceled" {
                cancelled += 1;
            } else {
                active += 1;
            }
            new_raws.push(raw_ev);

            match existing_by_id.get(occ.id.as_str()) {
                None => {
                    // Brand-new event.
                    truly_new.push(occ);
                }
                Some(prev) if prev.status != occ.status => {
                    // Status changed (e.g. active→canceled): needs upsert.
                    needs_rewrite = true;
                }
                Some(_) => {
                    // Unchanged — no write needed for contract row (still
                    // counted above for totals). But we still re-append raw
                    // so the latest invitee state is visible.
                }
            }
        }

        // Append truly-new contract rows.
        if !truly_new.is_empty() {
            contract_stream.append(&truly_new, |o| o.start.as_str())?;
        }

        // Rewrite the partition if any status changed.
        if needs_rewrite {
            // Build an updated map of all rows (existing + incoming updates).
            let incoming_map: HashMap<&str, &CalendarOccurrence> =
                incoming.iter().map(|(o, _)| (o.id.as_str(), o)).collect();
            let mut rewritten: Vec<CalendarOccurrence> = existing_rows
                .iter()
                .map(|r| {
                    if let Some(updated) = incoming_map.get(r.id.as_str()) {
                        // Replace with the fresher version.
                        (*updated).clone()
                    } else {
                        r.clone()
                    }
                })
                .collect();
            // Also append any truly-new rows that aren't in existing yet
            // (they were already added to truly_new above; skip double-add
            // by only including rows absent from existing_by_id).
            for occ in &truly_new {
                if !existing_by_id.contains_key(occ.id.as_str()) {
                    rewritten.push((*occ).clone());
                }
            }
            let rel = format!("{CONTRACT_DIR}/{key}.jsonl");
            vault.write_snapshot(&rel, &rewritten)?;
        }

        // Always re-append raw (last line = freshest state; reader dedupes
        // by uri using last-wins or full-fidelity replay).
        raw_stream.append(&new_raws, |r| {
            r.event
                .get("start_time")
                .and_then(Value::as_str)
                .unwrap_or("")
        })?;
    }

    let total = active + cancelled;
    Ok((total, active, cancelled))
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the token and sync. Public so `def_pull` and tests can call it.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context(
            "Calendly is not connected — paste your personal access token in the \
             Integrations tab",
        )?;
    let client = CalendlyClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl CalendlyApi) -> Result<PullOutcome> {
    // Step 1: get the user URI (required param for /scheduled_events).
    let user_uri = api
        .get("/users/me")
        .map_err(|e| match e {
            CalendlyError::Unauthorized => {
                anyhow::anyhow!(
                    "Calendly rejected the token (401) — reconnect from the Integrations tab"
                )
            }
            e => anyhow::anyhow!("Calendly /users/me failed: {e}"),
        })?;
    let user_uri_str = user_uri
        .pointer("/resource/uri")
        .and_then(Value::as_str)
        .context("Calendly /users/me: missing resource.uri")?
        .to_string();

    let state = vault.read_calendly_sync();

    // Step 2: Build the events query window.
    //
    // IMPORTANT: We always use a ROLLING window [now-LOOKBACK, now+LOOKAHEAD]
    // and never advance the lower bound past 'now' via a stored watermark.
    //
    // Why: if a future event (e.g. a booking 60 days out) were used as the
    // next min_start_time, every new booking made for dates between now and
    // that future event would fall below the floor and be silently lost. See
    // the "lookahead poison" note in the module-level doc comment.
    //
    // The rolling window also picks up status changes (cancellations) for
    // events that were already stored — those are upserted by write_layers.
    let now = Local::now();
    // Normalise to the documented-safe format: %Y-%m-%dT%H:%M:%SZ (no
    // fractional seconds). Calendly is documented to be picky about datetime
    // format; fractional-second forms have caused "Invalid Argument" errors
    // in community reports.
    let min_start_str = (now - chrono::Duration::days(LOOKBACK_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let max_start_str = (now + chrono::Duration::days(LOOKAHEAD_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    // percent-encode the user URI for safe inclusion in a URL query param.
    let encoded_user = percent_encode(&user_uri_str);
    let events_path = format!(
        "/scheduled_events?user={encoded_user}&count={PAGE_SIZE}\
         &min_start_time={min_start_str}&max_start_time={max_start_str}&status=active,canceled"
    );

    // Step 3: Drain all event pages.
    let raw_events: Vec<Value> = drain_pages(api, &events_path).map_err(|e| match e {
        CalendlyError::Unauthorized => {
            anyhow::anyhow!(
                "Calendly rejected the token (401) — reconnect from the Integrations tab"
            )
        }
        CalendlyError::RateLimited => {
            anyhow::anyhow!("Calendly rate limited the sync — it resumes next pass")
        }
        e => anyhow::anyhow!("Calendly /scheduled_events failed: {e}"),
    })?;

    // Step 4: Load existing contract rows (all partitions) for upsert comparison.
    let existing = existing_by_partition(vault)?;
    // Build a flat id→status map for quick lookup.
    let existing_status: std::collections::HashMap<String, String> = existing
        .values()
        .flat_map(|rows| rows.iter().map(|r| (r.id.clone(), r.status.clone())))
        .collect();

    // Step 5: For each event in the window, decide if it needs to be
    // fetched (new or status-changed). Events whose status is unchanged
    // and already stored still get their invitees re-fetched so the raw
    // layer stays fresh, but the contract row is not re-written.
    let mut rows: Vec<(CalendarOccurrence, RawEvent)> = Vec::new();
    for event in &raw_events {
        let uri = event.get("uri").and_then(Value::as_str).unwrap_or("");
        if uri.is_empty() {
            continue;
        }
        let uuid = uuid_from_uri(uri);
        let contract_id = format!("cly:{uuid}");

        // Determine the current API status for this event.
        let api_status = event
            .get("status")
            .and_then(Value::as_str)
            .map(|s| match s {
                "canceled" | "cancelled" => "canceled",
                _ => "confirmed",
            })
            .unwrap_or("confirmed");

        // Stored status (if any).
        let stored_status = existing_status.get(&contract_id).map(String::as_str);

        // Skip events that are already stored AND whose status hasn't changed.
        // This avoids unnecessary invitee API calls for unchanged events.
        if stored_status == Some(api_status) {
            continue;
        }

        // Step 5a: Fetch invitees for this event.
        let invitee_path =
            format!("/scheduled_events/{uuid}/invitees?count={PAGE_SIZE}");
        let raw_invitees: Vec<Value> = drain_pages(api, &invitee_path)
            .unwrap_or_default(); // graceful: if invitees fail, store event with no attendees

        // Step 5b: Build contract row.
        let Some(occ) = occ_from_event(event, &raw_invitees) else {
            continue;
        };

        // Step 5c: Build raw row.
        let event_map = match event {
            Value::Object(m) => m.clone(),
            _ => Map::new(),
        };
        let inv_maps: Vec<Map<String, Value>> = raw_invitees
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .collect();
        let raw_ev = RawEvent {
            uri: uri.to_string(),
            event: event_map,
            invitees: inv_maps,
        };
        rows.push((occ, raw_ev));
    }

    // Step 6: Write — cursor advances only after a successful write.
    let (total, active, cancelled) = write_layers(vault, rows, &existing)?;

    // Persist sync state (initialized flag + timestamp).
    let mut new_state = state;
    new_state.initialized = true;
    new_state.updated = Some(Local::now().to_rfc3339());
    vault.write_calendly_sync(&new_state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("events", total);
    counts.insert("active", active);
    counts.insert("cancelled", cancelled);
    Ok(PullOutcome {
        headline: format!(
            "Calendly synced — {total} events ({active} active, {cancelled} cancelled)"
        ),
        counts,
    })
}

/// Minimal percent-encoding for URL query parameter values. Encodes only the
/// characters that would break a query string (`%`, `&`, `+`, `=`, space).
/// The Calendly user URI is a URL itself
/// (`https://api.calendly.com/users/<uuid>`), so `:`, `/`, `.` must be
/// encoded too.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-calendly-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — modelled on the documented Calendly v2 API response shape.
    // Source: developer.calendly.com/api-docs  (v2 REST)
    //
    // Confirmed field names:
    //   Event: uri, name, start_time (RFC3339), end_time (RFC3339),
    //          status ("active"|"canceled"), event_type (uri),
    //          location { type, location|join_url }
    //   Invitee: uri, email, name, status, timezone, questions_and_answers
    //   List response: { collection: [...], pagination: { next_page_token } }

    fn event_active() -> Value {
        json!({
            "uri": "https://api.calendly.com/scheduled_events/AAA111BBB",
            "name": "30 Minute Meeting",
            "status": "active",
            "start_time": "2026-06-10T15:00:00.000000Z",
            "end_time": "2026-06-10T15:30:00.000000Z",
            "event_type": "https://api.calendly.com/event_types/EET111",
            "location": {
                "type": "zoom",
                "join_url": "https://zoom.us/j/12345",
                "location": ""
            }
        })
    }

    fn event_cancelled() -> Value {
        json!({
            "uri": "https://api.calendly.com/scheduled_events/CCC222DDD",
            "name": "15 Minute Chat",
            "status": "canceled",
            "start_time": "2026-06-11T10:00:00.000000Z",
            "end_time": "2026-06-11T10:15:00.000000Z",
            "event_type": "https://api.calendly.com/event_types/EET222",
            "location": {
                "type": "in_person",
                "location": "123 Main St"
            },
            "cancellation": {
                "reason": "invitee_cancellation"
            }
        })
    }

    fn invitee_alice() -> Value {
        json!({
            "uri": "https://api.calendly.com/scheduled_events/AAA111BBB/invitees/INV001",
            "email": "alice@example.com",
            "name": "Alice Smith",
            "status": "active",
            "timezone": "America/Los_Angeles",
            "questions_and_answers": [
                { "question": "What would you like to discuss?", "answer": "Product roadmap" }
            ]
        })
    }

    fn events_page(events: Vec<Value>, next_token: Option<&str>) -> Value {
        json!({
            "collection": events,
            "pagination": {
                "count": 2,
                "next_page": Value::Null,
                "next_page_token": next_token.map(|s| Value::String(s.to_string())).unwrap_or(Value::Null)
            }
        })
    }

    fn invitees_page(invitees: Vec<Value>) -> Value {
        let count = invitees.len();
        json!({
            "collection": invitees,
            "pagination": {
                "count": count,
                "next_page_token": Value::Null
            }
        })
    }

    fn user_me() -> Value {
        json!({
            "resource": {
                "uri": "https://api.calendly.com/users/USR001",
                "email": "test@example.com"
            }
        })
    }

    // -----------------------------------------------------------------------
    // Script helper: routes API calls to canned responses by URL prefix.

    struct ScriptedApi {
        user_me: Value,
        events: Value,
        invitees: std::collections::HashMap<String, Value>,
    }

    impl CalendlyApi for ScriptedApi {
        fn get(&self, path: &str) -> Result<Value, CalendlyError> {
            if path.starts_with("/users/me") {
                return Ok(self.user_me.clone());
            }
            if path.starts_with("/scheduled_events") && path.contains("/invitees") {
                // Extract UUID from /scheduled_events/<uuid>/invitees
                let uuid = path
                    .trim_start_matches("/scheduled_events/")
                    .split('/')
                    .next()
                    .unwrap_or("");
                return Ok(self
                    .invitees
                    .get(uuid)
                    .cloned()
                    .unwrap_or_else(|| json!({"collection":[],"pagination":{"next_page_token":null}})));
            }
            if path.starts_with("/scheduled_events") {
                return Ok(self.events.clone());
            }
            Err(CalendlyError::Other(format!("unhandled path: {path}")))
        }
    }

    // -----------------------------------------------------------------------
    // Unit tests.

    #[test]
    fn occ_from_active_event_maps_fields() {
        let event = event_active();
        let inv = vec![invitee_alice()];
        let occ = occ_from_event(&event, &inv).expect("should produce an occurrence");

        assert_eq!(occ.id, "cly:AAA111BBB");
        assert_eq!(occ.title, "30 Minute Meeting");
        assert_eq!(occ.status, "confirmed");
        assert!(!occ.all_day);
        assert!(!occ.recurring);
        // start_time converts from UTC to local; check prefix only.
        assert!(occ.start.starts_with("2026-06-10"), "start={}", occ.start);
        assert!(occ.end.starts_with("2026-06-10"), "end={}", occ.end);
        assert_eq!(occ.location, "https://zoom.us/j/12345");
        assert_eq!(occ.calendar, "Calendly");
        assert_eq!(occ.attendees, vec!["Alice Smith"]);
    }

    #[test]
    fn occ_from_cancelled_event_maps_status() {
        let event = event_cancelled();
        let occ = occ_from_event(&event, &[]).expect("should produce an occurrence");
        assert_eq!(occ.id, "cly:CCC222DDD");
        assert_eq!(occ.status, "canceled");
        assert_eq!(occ.location, "123 Main St");
        assert!(occ.attendees.is_empty());
    }

    #[test]
    fn occ_missing_start_time_returns_none() {
        let event = json!({ "uri": "https://api.calendly.com/scheduled_events/XYZ" });
        assert!(occ_from_event(&event, &[]).is_none());
    }

    #[test]
    fn occ_missing_uri_returns_none() {
        let event = json!({ "start_time": "2026-06-10T10:00:00Z", "status": "active" });
        assert!(occ_from_event(&event, &[]).is_none());
    }

    #[test]
    fn pull_with_scripted_api_writes_contract_and_raw() {
        let vault = temp_vault("pull_basic");
        let api = ScriptedApi {
            user_me: user_me(),
            events: events_page(vec![event_active(), event_cancelled()], None),
            invitees: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "AAA111BBB".to_string(),
                    invitees_page(vec![invitee_alice()]),
                );
                m
            },
        };
        let out = pull_with(&vault, &api).expect("pull should succeed");
        assert_eq!(out.counts.get("events").copied().unwrap_or(0), 2);
        assert_eq!(out.counts.get("active").copied().unwrap_or(0), 1);
        assert_eq!(out.counts.get("cancelled").copied().unwrap_or(0), 1);

        // Contract rows should exist.
        let contract = vault.stream(CONTRACT_DIR, Partition::Month);
        let mut total_rows = 0u64;
        for key in contract.partitions().unwrap() {
            total_rows += contract.read::<Value>(&key).unwrap().len() as u64;
        }
        assert_eq!(total_rows, 2, "two contract rows written");

        // Raw rows should exist.
        let raw = vault.stream(RAW_DIR, Partition::Month);
        let mut raw_rows = 0u64;
        for key in raw.partitions().unwrap() {
            raw_rows += raw.read::<Value>(&key).unwrap().len() as u64;
        }
        assert_eq!(raw_rows, 2, "two raw rows written");
    }

    #[test]
    fn pull_is_idempotent_on_second_run() {
        let vault = temp_vault("idempotent");
        let api = ScriptedApi {
            user_me: user_me(),
            events: events_page(vec![event_active()], None),
            invitees: std::collections::HashMap::new(),
        };
        pull_with(&vault, &api).expect("first pull");
        pull_with(&vault, &api).expect("second pull");

        let contract = vault.stream(CONTRACT_DIR, Partition::Month);
        let mut total = 0u64;
        for key in contract.partitions().unwrap() {
            total += contract.read::<Value>(&key).unwrap().len() as u64;
        }
        assert_eq!(total, 1, "no duplicate rows on re-pull");
    }

    #[test]
    fn sync_state_sets_initialized_after_pull() {
        let vault = temp_vault("cursor");
        let api = ScriptedApi {
            user_me: user_me(),
            events: events_page(vec![event_active(), event_cancelled()], None),
            invitees: std::collections::HashMap::new(),
        };
        assert!(!vault.read_calendly_sync().initialized, "not yet initialized");
        pull_with(&vault, &api).expect("pull");
        let state = vault.read_calendly_sync();
        assert!(state.initialized, "initialized flag set after pull");
        assert!(state.updated.is_some(), "updated timestamp set");
    }

    /// Regression: lookahead poison — a future-dated event must NOT advance
    /// min_start_time past now, stranding near-term bookings.
    ///
    /// We verify this by confirming the second pull (which now uses a rolling
    /// [now-365d, now+365d] window, not a stored watermark) still picks up
    /// events in the near-term window even after a far-future event has been seen.
    #[test]
    fn rolling_window_always_covers_near_term_bookings() {
        let vault = temp_vault("rolling_window");

        // First pull sees one event 365 days in the future.
        let future_event = json!({
            "uri": "https://api.calendly.com/scheduled_events/FUTURE001",
            "name": "Far Future Meeting",
            "status": "active",
            // The exact date doesn't matter; what matters is that the old
            // watermark-based logic would have set min_start_time to this
            // value, stranding all near-term bookings below it.
            "start_time": "2027-06-10T10:00:00.000000Z",
            "end_time": "2027-06-10T10:30:00.000000Z",
            "event_type": "https://api.calendly.com/event_types/EET999",
            "location": { "type": "zoom", "join_url": "https://zoom.us/j/99999", "location": "" }
        });

        let api1 = ScriptedApi {
            user_me: user_me(),
            events: events_page(vec![future_event], None),
            invitees: std::collections::HashMap::new(),
        };
        pull_with(&vault, &api1).expect("first pull with future event");

        // Second pull: a near-term booking arrives (today + 5 days).
        // With a watermark-based cursor this would be BELOW the cursor (T+365d)
        // and would be silently skipped. With a rolling window it is within
        // [now-365d, now+365d] and must be picked up.
        let near_term_event = json!({
            "uri": "https://api.calendly.com/scheduled_events/NEARTERM001",
            "name": "Near-Term Meeting",
            "status": "active",
            "start_time": "2026-06-21T14:00:00.000000Z",
            "end_time": "2026-06-21T14:30:00.000000Z",
            "event_type": "https://api.calendly.com/event_types/EET111",
            "location": { "type": "zoom", "join_url": "https://zoom.us/j/12345", "location": "" }
        });

        // The second pull's events page includes BOTH (rolling window covers both).
        let api2 = ScriptedApi {
            user_me: user_me(),
            events: events_page(vec![near_term_event], None),
            invitees: std::collections::HashMap::new(),
        };
        pull_with(&vault, &api2).expect("second pull with near-term event");

        // Both events must be in the vault.
        let contract = vault.stream(CONTRACT_DIR, Partition::Month);
        let total: usize = contract
            .partitions()
            .unwrap()
            .iter()
            .map(|k| contract.read::<Value>(k).unwrap().len())
            .sum();
        assert!(total >= 2, "both future and near-term events stored (got {total})");
    }

    /// Regression: status changes must be upserted.
    /// An event stored as "confirmed" that the API now reports as "canceled"
    /// must have its vault row updated, not silently kept as "confirmed".
    #[test]
    fn cancellation_after_first_capture_is_upserted() {
        let vault = temp_vault("upsert_cancel");

        // Pull 1: event_active stored as "confirmed".
        let api1 = ScriptedApi {
            user_me: user_me(),
            events: events_page(vec![event_active()], None),
            invitees: std::collections::HashMap::new(),
        };
        pull_with(&vault, &api1).expect("first pull");

        // Verify it is stored as confirmed.
        let contract = vault.stream(CONTRACT_DIR, Partition::Month);
        let first_rows: Vec<CalendarOccurrence> = contract
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| contract.read::<CalendarOccurrence>(k).unwrap())
            .collect();
        assert_eq!(first_rows.len(), 1);
        assert_eq!(first_rows[0].status, "confirmed");

        // Pull 2: same event is now canceled in the API.
        let canceled_event = json!({
            "uri": "https://api.calendly.com/scheduled_events/AAA111BBB",
            "name": "30 Minute Meeting",
            "status": "canceled",
            "start_time": "2026-06-10T15:00:00.000000Z",
            "end_time": "2026-06-10T15:30:00.000000Z",
            "event_type": "https://api.calendly.com/event_types/EET111",
            "location": { "type": "zoom", "join_url": "https://zoom.us/j/12345", "location": "" },
            "cancellation": { "reason": "invitee_cancellation" }
        });
        let api2 = ScriptedApi {
            user_me: user_me(),
            events: events_page(vec![canceled_event], None),
            invitees: std::collections::HashMap::new(),
        };
        pull_with(&vault, &api2).expect("second pull");

        // The stored row must now reflect "canceled".
        let second_rows: Vec<CalendarOccurrence> = contract
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| contract.read::<CalendarOccurrence>(k).unwrap())
            .collect();
        assert_eq!(second_rows.len(), 1, "exactly one row (not duplicated)");
        assert_eq!(
            second_rows[0].status, "canceled",
            "status updated from confirmed to canceled after second pull"
        );
    }

    #[test]
    fn drain_pages_follows_next_page_token() {
        // Two-page response: first page has a next_page_token, second doesn't.
        let page1 = json!({
            "collection": [{ "uri": "https://api.calendly.com/scheduled_events/EV001" }],
            "pagination": { "next_page_token": "tok_abc" }
        });
        let page2 = json!({
            "collection": [{ "uri": "https://api.calendly.com/scheduled_events/EV002" }],
            "pagination": { "next_page_token": null }
        });

        struct TwoPageApi { page1: Value, page2: Value }
        impl CalendlyApi for TwoPageApi {
            fn get(&self, path: &str) -> Result<Value, CalendlyError> {
                if path.contains("page_token=tok_abc") {
                    Ok(self.page2.clone())
                } else {
                    Ok(self.page1.clone())
                }
            }
        }
        let api = TwoPageApi { page1, page2 };
        let items = drain_pages(&api, "/scheduled_events?count=1").unwrap();
        assert_eq!(items.len(), 2, "both pages drained");
        assert_eq!(
            items[0].get("uri").and_then(Value::as_str).unwrap(),
            "https://api.calendly.com/scheduled_events/EV001"
        );
        assert_eq!(
            items[1].get("uri").and_then(Value::as_str).unwrap(),
            "https://api.calendly.com/scheduled_events/EV002"
        );
    }

    #[test]
    fn percent_encode_encodes_url_chars() {
        let uri = "https://api.calendly.com/users/ABC123";
        let encoded = percent_encode(uri);
        assert!(!encoded.contains("://"), "colon+slash should be encoded");
        assert!(encoded.contains("ABC123"), "path part preserved");
    }

    #[test]
    fn uuid_from_uri_extracts_last_segment() {
        assert_eq!(
            uuid_from_uri("https://api.calendly.com/scheduled_events/MY-UUID-123"),
            "MY-UUID-123"
        );
        assert_eq!(uuid_from_uri("bare"), "bare");
    }

    #[test]
    fn invitee_name_falls_back_to_email() {
        let event = event_active();
        let inv = vec![json!({
            "uri": "https://api.calendly.com/scheduled_events/AAA111BBB/invitees/INV999",
            "email": "bob@example.com",
            "name": "",
            "status": "active"
        })];
        let occ = occ_from_event(&event, &inv).unwrap();
        assert_eq!(occ.attendees, vec!["bob@example.com"]);
    }

    #[test]
    fn sync_state_round_trip() {
        let v = temp_vault("state");
        let default_state = v.read_calendly_sync();
        assert!(!default_state.initialized, "initialized starts false");
        assert!(default_state.updated.is_none());
        let state = SyncState {
            initialized: true,
            updated: Some("2026-06-16T08:00:00-07:00".to_string()),
        };
        v.write_calendly_sync(&state).unwrap();
        let loaded = v.read_calendly_sync();
        assert!(loaded.initialized, "initialized persists");
        assert_eq!(loaded.updated.as_deref(), Some("2026-06-16T08:00:00-07:00"));
    }
}
