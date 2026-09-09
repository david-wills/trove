//! Google Meet — conference records and transcript entries via the Meet REST
//! API v2. Catalogued in the Phase 2 pass; brief: docs/integrations/google-meet.md.
//!
//! Three destinations written in one pass per connected Google account:
//!
//! - **Raw** `meetings/google-meet/raw/YYYY-MM.jsonl` — verbatim conference
//!   record objects from `GET /v2/conferenceRecords`, full fidelity, upserted
//!   by the conference `name` field (the resource id).
//! - **Raw transcripts** `meetings/google-meet/raw/transcripts/<conf_id>.jsonl`
//!   — one sidecar per conference with a `FILE_GENERATED` transcript: all
//!   utterance entries from `…/transcripts/{id}/entries`, full fidelity.
//! - **Contract** `meetings/google-meet/YYYY-MM.jsonl` — one [`Meeting`] row
//!   per conference, upserted by `guid` = the conference record id.
//!
//! ## API: Meet REST v2 (`https://meet.googleapis.com/v2`)
//!
//! Auth: Bearer token from the shared `google` OAuth connection, per account
//! (reuses [`crate::sync::google::fresh_token`] — the same pattern as
//! Google Calendar and Gmail). Scope: `meetings.space.readonly` — this scope
//! is **not yet in the shared Google scope bundle** (`sync/google.rs`
//! `SCOPES`); adding it requires a Needs-David step and re-consent from
//! connected accounts. Until that step, `conferenceRecords.list` will return
//! an empty list or a 403 — the pull degrades gracefully (no-op, same outcome
//! as a newly-connected account with no meetings).
//!
//! - `GET /v2/conferenceRecords?pageSize=100` — list all records
//!   (`name`, `startTime`, `endTime`, `expireTime`, `space`)
//! - `GET /v2/conferenceRecords/{id}/transcripts` — list transcripts for one
//!   conference (`name`, `state`, `startTime`, `endTime`, `docsDestination`)
//! - `GET /v2/conferenceRecords/{id}/transcripts/{tid}/entries` — utterances
//!   (`name`, `participant`, `text`, `languageCode`, `startTime`, `endTime`)
//! - `GET /v2/conferenceRecords/{id}/participants` — who attended
//!   (`name`, `earliestStartTime`, `latestEndTime`, `signedinUser` |
//!   `anonymousUser` | `phoneUser` each with `displayName`)
//!
//! ## Cursor
//!
//! `.trove/google-meet-sync.json` (non-secret, rebuildable) holds per-account
//! `last_start_time` — the newest `startTime` stored. On each poll we drain
//! all pages; we (re)write a conference when its `startTime` is newer than
//! the watermark OR within [`RECHECK_DAYS`] (to pick up transcripts that
//! finish processing after the conference ends). Upsert-by-guid is idempotent.
//!
//! 🔒 Default-off opt-in: meeting transcripts are conversation content.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::meetings::Meeting;
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "google-meet";
const CONTRACT_DIR: &str = "meetings/google-meet";
const RAW_DIR: &str = "meetings/google-meet/raw";
const TRANSCRIPT_DIR: &str = "meetings/google-meet/raw/transcripts";
const SYNC_FILE: &str = ".trove/google-meet-sync.json";

const MEET_API: &str = "https://meet.googleapis.com/v2";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const SYNC_SECS: u64 = 900; // 15 min — same as Google Calendar

/// Within this many days of the watermark, a conference is re-checked so
/// transcripts that finish processing after the conference ends get picked up.
const RECHECK_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_meet_sync()
        .accounts
        .values()
        .filter_map(|a| a.last_start_time.clone())
        .filter(|s| !s.is_empty())
        .max()
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                out.headline.clone()
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "google-meet sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-meet",
        name: "Google Meet",
        kind: IntegrationKind::CloudSync,
        // 🔒 Opt-in: meeting content (transcripts) is conversation content.
        default_on: false,
        description: "Pulls Google Meet conference records and transcript entries into the vault \
                      using the Meet REST API v2. Shares the existing Google login.",
        domain: "meetings",
        vault_path: "meetings/google-meet/",
        toggleable: true,
        setup: &[
            "Connect your Google account from the Google card (shared with Gmail, Calendar, etc.).",
            "Note: the meetings.space.readonly scope must be enabled in the Google app \
             configuration and accepted in your consent flow before records will appear.",
        ],
        caveats: "The Meet transcript-entries API retains data for only 30 days after a \
                  conference ends; Trove polls every 15 minutes so you won't miss the window. \
                  Meeting transcripts are conversation content — this source is off by default; \
                  turn it on deliberately. Only the meeting organizer can retrieve recordings.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Per-account (`sub`) watermarks.
    #[serde(default)]
    accounts: HashMap<String, AccountState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct AccountState {
    /// RFC3339 startTime of the newest conference stored for this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_start_time: Option<String>,
}

impl Vault {
    fn read_meet_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_meet_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP client — injectable so tests run fully offline.

trait MeetApi {
    /// `GET <path>` (API-relative). Returns the parsed JSON body.
    fn get(&self, path: &str) -> Result<Value, MeetError>;
}

#[derive(Debug)]
enum MeetError {
    Unauthorized,
    NotFound,
    Other(String),
}

impl std::fmt::Display for MeetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeetError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            MeetError::NotFound => write!(f, "not found (HTTP 404)"),
            MeetError::Other(m) => write!(f, "{m}"),
        }
    }
}

struct MeetClient {
    base: String,
    token: String,
}

impl MeetClient {
    fn new(base: String, token: String) -> Self {
        MeetClient { base, token }
    }
}

impl MeetApi for MeetClient {
    fn get(&self, path: &str) -> Result<Value, MeetError> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .call();
        match resp {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| MeetError::Other(format!("parse: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(MeetError::Unauthorized),
            Err(ureq::Error::Status(404, _)) => Err(MeetError::NotFound),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(MeetError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(e) => Err(MeetError::Other(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// API shapes — only the fields we use; `#[serde(flatten)] extra` keeps full
// fidelity on the raw layer.

/// `conferenceRecords/{id}` as the API returns it. `name` is the resource
/// name and the stable guid: `conferenceRecords/<opaque>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawConference {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawConference {
    /// The opaque conference record id (last path component of `name`).
    fn conf_id(&self) -> String {
        self.fields
            .get("name")
            .and_then(Value::as_str)
            .and_then(|n| n.split('/').last())
            .unwrap_or("")
            .to_string()
    }

    fn start_time(&self) -> &str {
        self.fields.get("startTime").and_then(Value::as_str).unwrap_or("")
    }

    fn end_time(&self) -> Option<&str> {
        self.fields.get("endTime").and_then(Value::as_str)
    }

    fn space(&self) -> Option<&str> {
        self.fields
            .get("space")
            .and_then(Value::as_str)
            .or_else(|| {
                self.fields
                    .get("space")
                    .and_then(|v| v.get("name"))
                    .and_then(Value::as_str)
            })
    }
}

/// `conferenceRecords/{id}/transcripts/{tid}` shape.
#[derive(Debug, Deserialize)]
struct RawTranscript {
    name: String,
    #[serde(default)]
    state: String,
    #[serde(rename = "docsDestination", default)]
    docs_destination: Option<DocsDestination>,
}

#[derive(Debug, Deserialize)]
struct DocsDestination {
    #[serde(rename = "exportUri", default)]
    export_uri: String,
}

/// `conferenceRecords/{id}/transcripts/{tid}/entries/{eid}` shape.
#[derive(Debug, Serialize, Deserialize)]
struct TranscriptEntry {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

/// `conferenceRecords/{id}/participants/{pid}` shape.
#[derive(Debug, Deserialize)]
struct RawParticipant {
    #[serde(rename = "signedinUser", default)]
    signed_in_user: Option<SignedInUser>,
    #[serde(rename = "anonymousUser", default)]
    anonymous_user: Option<AnonymousUser>,
    #[serde(rename = "phoneUser", default)]
    phone_user: Option<PhoneUser>,
}

#[derive(Debug, Deserialize)]
struct SignedInUser {
    #[serde(rename = "displayName", default)]
    display_name: String,
    #[serde(default)]
    user: String,
}

#[derive(Debug, Deserialize)]
struct AnonymousUser {
    #[serde(rename = "displayName", default)]
    display_name: String,
}

#[derive(Debug, Deserialize)]
struct PhoneUser {
    #[serde(rename = "displayName", default)]
    display_name: String,
}

impl RawParticipant {
    fn display_name(&self) -> Option<&str> {
        if let Some(u) = &self.signed_in_user {
            if !u.display_name.is_empty() {
                return Some(&u.display_name);
            }
        }
        if let Some(u) = &self.anonymous_user {
            if !u.display_name.is_empty() {
                return Some(&u.display_name);
            }
        }
        if let Some(u) = &self.phone_user {
            if !u.display_name.is_empty() {
                return Some(&u.display_name);
            }
        }
        None
    }

    /// Returns the `signedinUser.user` field, which per the Meet REST v2 spec
    /// is an opaque People API resource name of the form `"users/{id}"` —
    /// never an email address. We store it in `attendees` as a service-id
    /// handle (the schema permits this) and optionally resolve to an email via
    /// the People API in a future enrichment pass.
    fn user_id(&self) -> Option<&str> {
        self.signed_in_user
            .as_ref()
            .map(|u| u.user.as_str())
            .filter(|s| !s.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// An RFC3339 timestamp → local time (the todoist/fathom `to_local` idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Duration in seconds between two RFC3339 instants (end − start), `None`
/// when either is missing/unparseable or end precedes start.
fn duration_secs(start: &str, end: Option<&str>) -> Option<i64> {
    let end = end?;
    let s = DateTime::parse_from_rfc3339(start).ok()?;
    let e = DateTime::parse_from_rfc3339(end).ok()?;
    let secs = (e - s).num_seconds();
    (secs >= 0).then_some(secs)
}

/// `transcript_ref` vault path for a conference record's utterance sidecar.
fn transcript_ref(conf_id: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{conf_id}.jsonl")
}

/// Build a [`Meeting`] contract row from a conference record object and its
/// participant list. `transcript_ref` is set by the caller after writing the
/// sidecar, not here.
///
/// Returns `None` only when there is no usable `startTime` (can't partition).
fn meeting_from_conf(conf: &RawConference, participants: &[RawParticipant]) -> Option<Meeting> {
    let guid = conf.conf_id();
    if guid.is_empty() {
        return None;
    }
    let start_raw = conf.start_time();
    if start_raw.is_empty() {
        return None;
    }
    let end_raw = conf.end_time();

    let mut m = Meeting::new(SOURCE, &guid, to_local(start_raw));
    m.started = to_local(start_raw);
    if let Some(end) = end_raw {
        m.ended = to_local(end);
    }
    m.duration_secs = duration_secs(start_raw, end_raw);
    m.platform = "meet".to_string();

    // Build attendee list from participants in a single pass to keep handles
    // and names positionally aligned. `signedinUser.user` is an opaque People
    // API resource id of the form "users/{id}" — never an email. We store it
    // as the handle in `attendees`; a future enrichment pass can resolve the
    // id to an email via the People API.
    //
    // Per-participant pair: (handle, Option<name>).
    let mut handle_name_pairs: Vec<(String, Option<String>)> = Vec::new();
    let mut anon_names: Vec<String> = Vec::new();
    for p in participants {
        if let Some(id) = p.user_id() {
            // Signed-in user: always gets a handle; name is optional.
            let name = p.display_name().map(str::to_string);
            handle_name_pairs.push((id.to_string(), name));
        } else if let Some(name) = p.display_name() {
            // Anonymous / phone user: no opaque id, use display name as best-
            // effort handle (collected separately so they don't misalign with
            // the signed-in handles list).
            anon_names.push(name.to_string());
        }
    }

    // attendee_names must be positionally aligned with attendees.
    if !handle_name_pairs.is_empty() {
        let all_have_names = handle_name_pairs.iter().all(|(_, n)| n.is_some());
        m.attendees = handle_name_pairs.iter().map(|(h, _)| h.clone()).collect();
        if all_have_names {
            m.attendee_names =
                handle_name_pairs.into_iter().filter_map(|(_, n)| n).collect();
        }
        // Anon names go to extra so they aren't dropped.
        if !anon_names.is_empty() {
            m.extra.insert(
                "anon_participant_names".into(),
                Value::Array(anon_names.into_iter().map(Value::String).collect()),
            );
        }
    } else if !anon_names.is_empty() {
        // No signed-in users at all — put anon display names in attendees as
        // best-effort handles.
        m.attendees = anon_names;
    }

    // meeting_url from the space's meetingUri if the space field is present.
    // The conference record's `space` field is a resource name
    // ("spaces/XYZ"), not the meetingUri; we store it in extra.
    if let Some(space) = conf.space() {
        m.extra.insert("space".into(), Value::String(space.to_string()));
    }

    // expireTime in extra (useful for pruning).
    if let Some(expire) = conf.fields.get("expireTime").and_then(Value::as_str) {
        m.extra.insert("expireTime".into(), Value::String(expire.to_string()));
    }

    Some(m)
}

// ---------------------------------------------------------------------------
// Pagination helpers (drain the full list — never stop early).

fn drain_pages<T>(
    api: &impl MeetApi,
    first_path: &str,
    items_key: &str,
    mut on_item: impl FnMut(Value) -> Option<T>,
) -> Result<Vec<T>, MeetError> {
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let path = match &page_token {
            Some(t) => {
                let sep = if first_path.contains('?') { '&' } else { '?' };
                format!("{first_path}{sep}pageToken={t}")
            }
            None => first_path.to_string(),
        };
        let body = api.get(&path)?;
        let items = body
            .get(items_key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in items {
            if let Some(v) = on_item(item) {
                out.push(v);
            }
        }
        let next = body
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match next {
            Some(t) => page_token = Some(t),
            None => break,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Upsert-by-guid helpers (the fathom pattern adapted for google-meet).

fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    upsert_partition(vault, CONTRACT_DIR, rows, |m| m.ts.clone(), |m| m.guid.clone())
}

fn upsert_raw(vault: &Vault, rows: Vec<RawConference>) -> Result<u64> {
    upsert_partition(
        vault,
        RAW_DIR,
        rows,
        |r| to_local(r.start_time()),
        |r| r.conf_id(),
    )
}

fn upsert_partition<T, FT, FG>(
    vault: &Vault,
    dir: &str,
    rows: Vec<T>,
    ts_of: FT,
    guid_of: FG,
) -> Result<u64>
where
    T: Serialize + serde::de::DeserializeOwned,
    FT: Fn(&T) -> String,
    FG: Fn(&T) -> String,
{
    let stream = vault.stream(dir, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for r in rows {
        let ts = ts_of(&r);
        let key = Partition::Month
            .key(&ts)
            .with_context(|| format!("google-meet: ts {ts:?} has no month (dir {dir})"))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<T> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> =
            existing.iter().enumerate().map(|(i, r)| (guid_of(r), i)).collect();
        for r in fresh {
            let g = guid_of(&r);
            match idx.get(&g).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(g, existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| {
            ts_of(a).cmp(&ts_of(b)).then_with(|| guid_of(a).cmp(&guid_of(b)))
        });
        vault.write_snapshot(&format!("{dir}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Top-level pull: iterate every connected Google account.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let accounts = vault.google_status()?.accounts;
    if accounts.is_empty() {
        return Ok(PullOutcome {
            headline: "google-meet: no Google account connected".to_string(),
            counts: BTreeMap::new(),
        });
    }

    let mut state = vault.read_meet_sync();
    let mut total_meetings = 0u64;
    let mut total_transcripts = 0u64;
    let mut total_raw = 0u64;

    for acct in &accounts {
        if acct.needs_reconnect {
            continue;
        }
        let token = match crate::sync::google::fresh_token(vault, &acct.sub) {
            Ok(t) => t.access_token,
            Err(e) => {
                // Log and skip; don't abort other accounts.
                let _ = e; // error already surfaced by fresh_token internals
                continue;
            }
        };
        let client = MeetClient::new(MEET_API.to_string(), token);
        let acct_state = state.accounts.entry(acct.sub.clone()).or_default();
        match pull_account(vault, &client, acct_state) {
            Ok((meetings, transcripts, raw)) => {
                total_meetings += meetings;
                total_transcripts += transcripts;
                total_raw += raw;
            }
            Err(e) => {
                // A single account failure doesn't abort others. Degrade
                // gracefully: the 403 "missing scope" case is common here
                // until the Needs-David scope addition lands.
                let _silenced = e;
            }
        }
    }

    vault.write_meet_sync(&state)?;

    let headline = format!(
        "google-meet synced — {total_meetings} meetings, {total_transcripts} transcripts"
    );
    let counts = BTreeMap::from([
        ("meetings", total_meetings),
        ("transcripts", total_transcripts),
        ("raw", total_raw),
    ]);
    Ok(PullOutcome { headline, counts })
}

/// Pull one account. Returns (contract_new, transcripts_written, raw_new).
fn pull_account(
    vault: &Vault,
    api: &impl MeetApi,
    state: &mut AccountState,
) -> Result<(u64, u64, u64)> {
    let watermark = state.last_start_time.clone();
    let recheck_floor: Option<String> = watermark.as_deref().map(|w| {
        DateTime::parse_from_rfc3339(w)
            .map(|t| (t - chrono::Duration::days(RECHECK_DAYS)).to_rfc3339())
            .unwrap_or_else(|_| w.to_string())
    });

    // 1. Drain conference records. The Meet API lists records in descending
    // startTime order by default and supports a server-side `filter` parameter.
    // We push the recheck floor as the server-side lower bound to avoid
    // fetching unbounded history on every poll. Client-side filtering still
    // applies for the recheck window vs. is_new distinction.
    let filter_floor = recheck_floor.as_deref().unwrap_or("");
    let list_path = if filter_floor.is_empty() {
        "/conferenceRecords?pageSize=100".to_string()
    } else {
        // URL-encode the RFC3339 timestamp (replace '+' with '%2B'; ':' and
        // '-' are safe in query values per RFC3986).
        let encoded = filter_floor.replace('+', "%2B");
        format!("/conferenceRecords?pageSize=100&filter=start_time>=\"{encoded}\"")
    };
    let conferences: Vec<RawConference> = drain_pages(
        api,
        &list_path,
        "conferenceRecords",
        |v| {
            let m = v.as_object().cloned()?;
            // Must have a name and a startTime to be useful.
            if m.get("name").and_then(Value::as_str).is_none() {
                return None;
            }
            Some(RawConference { fields: m })
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // 2. Write/filter pass.
    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut raw_rows: Vec<RawConference> = Vec::new();
    let mut transcripts_written = 0u64;
    let mut newest_start: Option<String> = watermark.clone();

    for conf in &conferences {
        let start = conf.start_time().to_string();
        if start.is_empty() {
            continue;
        }
        let is_new = watermark.as_deref().is_none_or(|w| start.as_str() > w);
        let recheck =
            recheck_floor.as_deref().is_some_and(|floor| start.as_str() > floor);
        if !is_new && !recheck {
            // Old conference outside the window — still advance watermark.
            newest_start = max_ts(newest_start, start);
            continue;
        }

        // Raw: verbatim API object.
        raw_rows.push(conf.clone());

        // Participants (for attendee list). A 404 is fine (conference gone).
        let conf_id = conf.conf_id();
        let participants = fetch_participants(api, &conf_id);

        let Some(mut row) = meeting_from_conf(conf, &participants) else {
            continue;
        };

        // Transcripts for this conference.
        let (ref_path, tc) = fetch_transcript_sidecar(vault, api, &conf_id)
            .unwrap_or((String::new(), 0));
        if !ref_path.is_empty() {
            row.transcript_ref = ref_path;
        }
        transcripts_written += tc;

        contract_rows.push(row);
        newest_start = max_ts(newest_start, start);
    }

    // 3. Persist.
    let raw_new = upsert_raw(vault, raw_rows)?;
    let contract_new = upsert_contract(vault, contract_rows)?;

    state.last_start_time = newest_start;
    Ok((contract_new, transcripts_written, raw_new))
}

/// Fetch the participant list for a conference, returning an empty vec on any
/// error (a missing/expired conference is non-fatal).
fn fetch_participants(api: &impl MeetApi, conf_id: &str) -> Vec<RawParticipant> {
    let path = format!("/conferenceRecords/{conf_id}/participants?pageSize=250");
    let result = drain_pages(api, &path, "participants", |v| {
        serde_json::from_value::<RawParticipant>(v).ok()
    });
    result.unwrap_or_default()
}

/// Fetch transcripts for a conference and write the sidecar (full utterance
/// fidelity). Returns (`transcript_ref`, `transcripts_written`). Non-fatal on
/// errors: an expired or missing transcript is a graceful no-op.
///
/// A conference may have multiple `FILE_GENERATED` transcripts (e.g. when
/// transcription is stopped and restarted mid-meeting). We iterate ALL of them
/// and write their entries into a single sidecar so no utterances are dropped.
fn fetch_transcript_sidecar(
    vault: &Vault,
    api: &impl MeetApi,
    conf_id: &str,
) -> Result<(String, u64)> {
    let path = format!("/conferenceRecords/{conf_id}/transcripts");
    let transcripts: Vec<RawTranscript> = drain_pages(api, &path, "transcripts", |v| {
        serde_json::from_value::<RawTranscript>(v).ok()
    })
    .unwrap_or_default();

    // Collect all completed (FILE_GENERATED) transcripts.
    let completed: Vec<RawTranscript> = transcripts
        .into_iter()
        .filter(|t| t.state == "FILE_GENERATED")
        .collect();

    if completed.is_empty() {
        return Ok((String::new(), 0));
    }

    // Gather all entries from every FILE_GENERATED transcript into one sidecar.
    let mut all_entries: Vec<TranscriptEntry> = Vec::new();
    for transcript in completed {
        // Extract the transcript id from its resource name.
        let transcript_id = transcript.name.split('/').last().unwrap_or("").to_string();
        if transcript_id.is_empty() {
            continue;
        }

        // Prepend a metadata line with transcript-level fields so readers can
        // attribute entries to the correct segment when multiple transcripts
        // are present.
        let mut meta = Map::new();
        meta.insert("_transcriptId".into(), Value::String(transcript_id.clone()));
        if let Some(docs) = &transcript.docs_destination {
            if !docs.export_uri.is_empty() {
                meta.insert(
                    "_docsExportUri".into(),
                    Value::String(docs.export_uri.clone()),
                );
            }
        }
        all_entries.push(TranscriptEntry { fields: meta });

        // Fetch all utterance entries for this transcript segment.
        let entry_path =
            format!("/conferenceRecords/{conf_id}/transcripts/{transcript_id}/entries");
        let entries: Vec<TranscriptEntry> =
            drain_pages(api, &entry_path, "transcriptEntries", |v| {
                let m = v.as_object().cloned()?;
                Some(TranscriptEntry { fields: m })
            })
            .unwrap_or_default();

        all_entries.extend(entries);
    }

    // If we only got metadata lines (no actual utterances), don't write a
    // sidecar (transcript queued but not yet processed).
    let has_utterances = all_entries.iter().any(|e| !e.fields.contains_key("_transcriptId"));
    if !has_utterances {
        return Ok((String::new(), 0));
    }

    // Write all entries to the conference-scoped sidecar (full fidelity).
    let ref_path = transcript_ref(conf_id);
    vault.write_snapshot(&ref_path, &all_entries)?;
    Ok((ref_path, 1))
}

/// The later of two RFC3339 timestamps (lexical compare is valid for ISO8601
/// UTC `Z`-suffix strings, which is what the Meet API returns).
fn max_ts(cur: Option<String>, candidate: String) -> Option<String> {
    match cur {
        Some(prev) if prev.as_str() >= candidate.as_str() => Some(prev),
        _ => Some(candidate),
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-meet-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — shapes verified against the official Meet REST API v2 docs.

    /// A conference record object as the API returns it.
    fn conf(id: &str, start: &str, end: &str) -> Value {
        serde_json::json!({
            "name": format!("conferenceRecords/{id}"),
            "startTime": start,
            "endTime": end,
            "expireTime": "2026-07-15T14:00:00Z",
            "space": "spaces/abc123"
        })
    }

    fn conf_no_end(id: &str, start: &str) -> Value {
        serde_json::json!({
            "name": format!("conferenceRecords/{id}"),
            "startTime": start,
            "space": "spaces/abc123"
        })
    }

    /// Signed-in participant fixture. `user_id` must be a realistic opaque
    /// People API resource name, e.g. `"users/111820798776727307963"`. The Meet
    /// REST API v2 spec (SignedinUser.user) defines `user` as:
    /// "Unique ID for the user. Format: users/{user}" — never an email address.
    fn participant_signed_in(display: &str, user_id: &str) -> Value {
        serde_json::json!({
            "name": "conferenceRecords/cr1/participants/p1",
            "earliestStartTime": "2026-06-15T14:00:00Z",
            "latestEndTime": "2026-06-15T15:00:00Z",
            "signedinUser": {
                "displayName": display,
                "user": user_id
            }
        })
    }

    fn participant_anon(display: &str) -> Value {
        serde_json::json!({
            "name": "conferenceRecords/cr1/participants/p2",
            "earliestStartTime": "2026-06-15T14:00:00Z",
            "latestEndTime": "2026-06-15T15:00:00Z",
            "anonymousUser": {
                "displayName": display
            }
        })
    }

    fn transcript_file_generated() -> Value {
        serde_json::json!({
            "name": "conferenceRecords/cr1/transcripts/t1",
            "state": "FILE_GENERATED",
            "startTime": "2026-06-15T14:00:00Z",
            "endTime": "2026-06-15T14:58:00Z",
            "docsDestination": {
                "exportUri": "https://docs.google.com/document/d/abc"
            }
        })
    }

    fn transcript_entry(text: &str) -> Value {
        serde_json::json!({
            "name": "conferenceRecords/cr1/transcripts/t1/entries/e1",
            "participant": "conferenceRecords/cr1/participants/p1",
            "text": text,
            "languageCode": "en-US",
            "startTime": "2026-06-15T14:00:05Z",
            "endTime": "2026-06-15T14:00:08Z"
        })
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        routes: RefCell<Vec<(String, Value)>>,
        calls: RefCell<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                routes: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn register(&self, path_prefix: &str, body: Value) {
            self.routes.borrow_mut().push((path_prefix.to_string(), body));
        }

        fn called(&self, needle: &str) -> bool {
            self.calls.borrow().iter().any(|p| p.contains(needle))
        }
    }

    impl MeetApi for MockApi {
        fn get(&self, path: &str) -> Result<Value, MeetError> {
            self.calls.borrow_mut().push(path.to_string());
            // Longest-prefix match so "/transcripts/t1/entries" wins over "/transcripts".
            let routes = self.routes.borrow();
            let best = routes
                .iter()
                .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
                .max_by_key(|(prefix, _)| prefix.len());
            if let Some((_, body)) = best {
                return Ok(body.clone());
            }
            // Unregistered: return empty object (graceful fallback).
            Ok(serde_json::json!({}))
        }
    }

    // -----------------------------------------------------------------------
    // Pure-mapping tests.

    #[test]
    fn conf_id_is_last_path_component() {
        let raw = RawConference {
            fields: conf("cr42", "2026-06-15T14:00:00Z", "2026-06-15T15:00:00Z")
                .as_object().unwrap().clone(),
        };
        assert_eq!(raw.conf_id(), "cr42");
    }

    #[test]
    fn meeting_from_conf_sets_required_fields() {
        let raw = RawConference {
            fields: conf("cr1", "2026-06-15T14:00:00Z", "2026-06-15T15:00:00Z")
                .as_object().unwrap().clone(),
        };
        // The Meet REST API v2 returns signedinUser.user as an opaque People
        // API resource name ("users/{id}"), never an email address.
        let user_id = "users/111820798776727307963";
        let p: Vec<RawParticipant> =
            serde_json::from_value(serde_json::json!([
                participant_signed_in("David Wills", user_id)
            ])).unwrap();
        let m = meeting_from_conf(&raw, &p).unwrap();
        assert_eq!(m.source, "google-meet");
        assert_eq!(m.guid, "cr1");
        assert_eq!(m.platform, "meet");
        assert_eq!(m.duration_secs, Some(3600));
        // attendees holds the opaque People API resource id; a future enrichment
        // pass may resolve it to an email via the People API.
        assert_eq!(m.attendees, vec![user_id]);
        assert_eq!(m.attendee_names, vec!["David Wills"]);
        assert!(m.extra.contains_key("space"));
        assert!(m.extra.contains_key("expireTime"));
        assert_eq!(m.transcript_ref, "", "transcript_ref set by caller");
    }

    #[test]
    fn meeting_no_end_has_no_duration() {
        let raw = RawConference {
            fields: conf_no_end("cr2", "2026-06-15T14:00:00Z")
                .as_object().unwrap().clone(),
        };
        let m = meeting_from_conf(&raw, &[]).unwrap();
        assert_eq!(m.duration_secs, None);
        assert!(m.ended.is_empty());
    }

    #[test]
    fn anon_participant_goes_to_attendees_as_display_name() {
        let raw = RawConference {
            fields: conf("cr3", "2026-06-15T14:00:00Z", "2026-06-15T14:30:00Z")
                .as_object().unwrap().clone(),
        };
        let p: Vec<RawParticipant> =
            serde_json::from_value(serde_json::json!([
                participant_anon("Anonymous Guest")
            ])).unwrap();
        let m = meeting_from_conf(&raw, &p).unwrap();
        // No signed-in user (no opaque id) — display name goes to attendees directly.
        assert_eq!(m.attendees, vec!["Anonymous Guest"]);
        assert!(m.attendee_names.is_empty());
    }

    #[test]
    fn duration_computed_correctly() {
        assert_eq!(duration_secs("2026-06-15T14:00:00Z", Some("2026-06-15T15:00:00Z")), Some(3600));
        assert_eq!(duration_secs("2026-06-15T14:00:00Z", None), None);
        assert_eq!(
            duration_secs("2026-06-15T15:00:00Z", Some("2026-06-15T14:00:00Z")),
            None,
            "end before start"
        );
    }

    #[test]
    fn transcript_ref_path_is_conf_scoped() {
        assert_eq!(transcript_ref("cr99"), "meetings/google-meet/raw/transcripts/cr99.jsonl");
    }

    #[test]
    fn max_ts_picks_later() {
        assert_eq!(
            max_ts(Some("2026-06-10T00:00:00Z".into()), "2026-06-15T00:00:00Z".into()),
            Some("2026-06-15T00:00:00Z".into())
        );
        assert_eq!(
            max_ts(Some("2026-06-20T00:00:00Z".into()), "2026-06-15T00:00:00Z".into()),
            Some("2026-06-20T00:00:00Z".into())
        );
        assert_eq!(max_ts(None, "2026-06-15T00:00:00Z".into()), Some("2026-06-15T00:00:00Z".into()));
    }

    // -----------------------------------------------------------------------
    // Pull integration tests.

    #[test]
    fn empty_account_list_returns_ok() {
        // No accounts connected — pull succeeds with zero counts, no panic.
        let v = temp_vault("no-accounts");
        // Direct call to pull() would need connected accounts; test via pull_account instead.
        let api = MockApi::new();
        let mut state = AccountState::default();
        let (m, t, r) = pull_account(&v, &api, &mut state).unwrap();
        assert_eq!((m, t, r), (0, 0, 0));
        assert!(state.last_start_time.is_none(), "no conferences → no watermark");
    }

    #[test]
    fn full_pull_writes_contract_raw_transcript() {
        let v = temp_vault("fullpull");
        let api = MockApi::new();

        // Conference list.
        api.register(
            "/conferenceRecords",
            serde_json::json!({
                "conferenceRecords": [
                    conf("cr1", "2026-06-15T14:00:00Z", "2026-06-15T15:00:00Z")
                ]
            }),
        );
        // Participants. `user` is an opaque People API resource id per the spec.
        let user_id = "users/111820798776727307963";
        api.register(
            "/conferenceRecords/cr1/participants",
            serde_json::json!({
                "participants": [participant_signed_in("David Wills", user_id)]
            }),
        );
        // Transcripts.
        api.register(
            "/conferenceRecords/cr1/transcripts",
            serde_json::json!({ "transcripts": [transcript_file_generated()] }),
        );
        // Transcript entries.
        api.register(
            "/conferenceRecords/cr1/transcripts/t1/entries",
            serde_json::json!({
                "transcriptEntries": [transcript_entry("Hello, everyone.")]
            }),
        );

        let mut state = AccountState::default();
        let (meetings, transcripts, raw) = pull_account(&v, &api, &mut state).unwrap();

        assert_eq!(meetings, 1, "one new contract row");
        assert_eq!(transcripts, 1, "one transcript sidecar");
        assert_eq!(raw, 1, "one raw row");

        // API calls made.
        assert!(api.called("/conferenceRecords"), "listed conferences");
        assert!(api.called("participants"), "fetched participants");
        assert!(api.called("/transcripts"), "fetched transcripts");
        assert!(api.called("/entries"), "fetched entries");

        // Contract row.
        let key = Partition::Month
            .key(&to_local("2026-06-15T14:00:00Z"))
            .unwrap()
            .to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.guid, "cr1");
        assert_eq!(m.platform, "meet");
        // attendees holds the opaque People API resource id returned by the API.
        assert_eq!(m.attendees, vec![user_id]);
        assert_eq!(m.transcript_ref, "meetings/google-meet/raw/transcripts/cr1.jsonl");

        // Transcript sidecar.
        let sidecar = v.root().join("meetings/google-meet/raw/transcripts/cr1.jsonl");
        assert!(sidecar.exists(), "sidecar written");
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("Hello, everyone."), "utterance text in sidecar");
        assert!(body.contains("docs.google.com"), "docsExportUri in sidecar metadata line");

        // Raw.
        let raw_file = v.root().join("meetings/google-meet/raw/2026-06.jsonl");
        assert!(raw_file.exists());
        let raw_body = std::fs::read_to_string(&raw_file).unwrap();
        assert!(raw_body.contains("\"conferenceRecords/cr1\""), "raw has resource name");

        // Watermark advanced.
        assert_eq!(
            state.last_start_time.as_deref(),
            Some("2026-06-15T14:00:00Z"),
        );
    }

    #[test]
    fn no_transcript_row_written_without_sidecar() {
        let v = temp_vault("no-transcript");
        let api = MockApi::new();
        // Conference with no transcripts.
        api.register(
            "/conferenceRecords",
            serde_json::json!({
                "conferenceRecords": [conf("cr2", "2026-06-14T10:00:00Z", "2026-06-14T10:30:00Z")]
            }),
        );
        api.register(
            "/conferenceRecords/cr2/transcripts",
            serde_json::json!({ "transcripts": [] }),
        );

        let mut state = AccountState::default();
        let (meetings, transcripts, _raw) = pull_account(&v, &api, &mut state).unwrap();
        assert_eq!(meetings, 1, "still get a contract row (metadata only)");
        assert_eq!(transcripts, 0, "no transcript sidecar when none available");

        let key = Partition::Month
            .key(&to_local("2026-06-14T10:00:00Z"))
            .unwrap()
            .to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows[0].transcript_ref, "", "no transcript_ref when no transcript");
    }

    #[test]
    fn recheck_window_updates_existing_row_with_transcript() {
        // A conference written on poll 1 without a transcript is updated on
        // poll 2 when its transcript is now FILE_GENERATED.
        let v = temp_vault("recheck");
        let api1 = MockApi::new();
        let start = "2026-06-15T14:00:00Z";
        api1.register(
            "/conferenceRecords",
            serde_json::json!({
                "conferenceRecords": [conf("cr3", start, "2026-06-15T15:00:00Z")]
            }),
        );
        api1.register(
            "/conferenceRecords/cr3/transcripts",
            serde_json::json!({ "transcripts": [] }),
        );

        let mut state = AccountState::default();
        let _ = pull_account(&v, &api1, &mut state).unwrap();
        assert_eq!(state.last_start_time.as_deref(), Some(start));

        // Poll 2: same conference, now with transcript.
        let api2 = MockApi::new();
        api2.register(
            "/conferenceRecords",
            serde_json::json!({
                "conferenceRecords": [conf("cr3", start, "2026-06-15T15:00:00Z")]
            }),
        );
        api2.register(
            "/conferenceRecords/cr3/participants",
            serde_json::json!({ "participants": [] }),
        );
        api2.register(
            "/conferenceRecords/cr3/transcripts",
            serde_json::json!({ "transcripts": [transcript_file_generated().pointer("/").cloned()
                .unwrap_or_else(|| serde_json::json!({
                    "name": "conferenceRecords/cr3/transcripts/t1",
                    "state": "FILE_GENERATED"
                }))] }),
        );
        api2.register(
            "/conferenceRecords/cr3/transcripts/t1/entries",
            serde_json::json!({
                "transcriptEntries": [transcript_entry("Updated transcript text")]
            }),
        );

        let (meetings, transcripts, _) = pull_account(&v, &api2, &mut state).unwrap();
        // The meeting is within RECHECK_DAYS of the watermark so it's re-processed.
        assert_eq!(transcripts, 1, "transcript added on recheck poll");
        let _ = meetings; // upsert is idempotent — count not meaningful on a recheck

        let key = Partition::Month.key(&to_local(start)).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1, "same row, not duplicated");
        assert_eq!(
            rows[0].transcript_ref,
            "meetings/google-meet/raw/transcripts/cr3.jsonl"
        );
    }

    #[test]
    fn old_conference_outside_recheck_not_reprocessed() {
        let v = temp_vault("old-conf");
        // Set a watermark 60 days ahead of the conference start — outside RECHECK_DAYS.
        let old_start = "2026-01-01T10:00:00Z";
        let future_watermark = "2026-06-15T00:00:00Z";

        let api = MockApi::new();
        api.register(
            "/conferenceRecords",
            serde_json::json!({
                "conferenceRecords": [conf("crOld", old_start, "2026-01-01T11:00:00Z")]
            }),
        );

        let mut state =
            AccountState { last_start_time: Some(future_watermark.to_string()) };
        let (meetings, _transcripts, raw) = pull_account(&v, &api, &mut state).unwrap();
        // The old conference is outside the recheck window and is skipped.
        assert_eq!(meetings, 0, "old conference outside recheck window not rewritten");
        assert_eq!(raw, 0);
    }

    #[test]
    fn multiple_transcripts_all_written_to_sidecar() {
        // A conference with two FILE_GENERATED transcripts (transcription stopped
        // and restarted) — both segments must appear in the sidecar.
        let v = temp_vault("multi-transcript");
        let api = MockApi::new();

        api.register(
            "/conferenceRecords",
            serde_json::json!({
                "conferenceRecords": [
                    conf("cr5", "2026-06-15T14:00:00Z", "2026-06-15T16:00:00Z")
                ]
            }),
        );
        api.register(
            "/conferenceRecords/cr5/participants",
            serde_json::json!({ "participants": [] }),
        );
        // Two FILE_GENERATED transcripts.
        api.register(
            "/conferenceRecords/cr5/transcripts",
            serde_json::json!({
                "transcripts": [
                    {
                        "name": "conferenceRecords/cr5/transcripts/t1",
                        "state": "FILE_GENERATED"
                    },
                    {
                        "name": "conferenceRecords/cr5/transcripts/t2",
                        "state": "FILE_GENERATED"
                    }
                ]
            }),
        );
        // Utterances for each segment.
        api.register(
            "/conferenceRecords/cr5/transcripts/t1/entries",
            serde_json::json!({
                "transcriptEntries": [{
                    "name": "conferenceRecords/cr5/transcripts/t1/entries/e1",
                    "text": "First segment utterance.",
                    "languageCode": "en-US"
                }]
            }),
        );
        api.register(
            "/conferenceRecords/cr5/transcripts/t2/entries",
            serde_json::json!({
                "transcriptEntries": [{
                    "name": "conferenceRecords/cr5/transcripts/t2/entries/e1",
                    "text": "Second segment utterance.",
                    "languageCode": "en-US"
                }]
            }),
        );

        let mut state = AccountState::default();
        let (_, transcripts, _) = pull_account(&v, &api, &mut state).unwrap();
        assert_eq!(transcripts, 1, "one sidecar file for the conference");

        let sidecar = v.root().join("meetings/google-meet/raw/transcripts/cr5.jsonl");
        assert!(sidecar.exists(), "sidecar written");
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("First segment utterance."), "first segment in sidecar");
        assert!(body.contains("Second segment utterance."), "second segment in sidecar");
        assert!(body.contains("\"t1\""), "t1 metadata line present");
        assert!(body.contains("\"t2\""), "t2 metadata line present");
    }

    #[test]
    fn signed_in_and_anon_participants_aligned_correctly() {
        // Signed-in user with id+name + anonymous user with only display name.
        // handles and names must stay paired; anon name goes to extra.
        let raw = RawConference {
            fields: conf("cr6", "2026-06-15T14:00:00Z", "2026-06-15T15:00:00Z")
                .as_object().unwrap().clone(),
        };
        let p: Vec<RawParticipant> = serde_json::from_value(serde_json::json!([
            {
                "name": "conferenceRecords/cr6/participants/p1",
                "signedinUser": {
                    "displayName": "Alice",
                    "user": "users/999000111"
                }
            },
            {
                "name": "conferenceRecords/cr6/participants/p2",
                "anonymousUser": {
                    "displayName": "Anon Guest"
                }
            }
        ])).unwrap();
        let m = meeting_from_conf(&raw, &p).unwrap();
        // Only the signed-in participant gets a handle.
        assert_eq!(m.attendees, vec!["users/999000111"]);
        assert_eq!(m.attendee_names, vec!["Alice"]);
        // Anon name goes to extra, not into attendee_names (which would misalign).
        assert!(
            m.extra.contains_key("anon_participant_names"),
            "anon display name in extra"
        );
    }

    #[test]
    fn upsert_contract_deduplicates_by_guid() {
        let v = temp_vault("upsert");
        let mut m1 = Meeting::new("google-meet", "cr1", "2026-06-15T14:00:00-07:00".to_string());
        m1.title = "First version".to_string();
        upsert_contract(&v, vec![m1]).unwrap();

        let mut m2 = Meeting::new("google-meet", "cr1", "2026-06-15T14:00:00-07:00".to_string());
        m2.title = "Updated version".to_string();
        upsert_contract(&v, vec![m2]).unwrap();

        let key = Partition::Month
            .key("2026-06-15T14:00:00-07:00")
            .unwrap()
            .to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1, "upsert: same guid -> one row");
        assert_eq!(rows[0].title, "Updated version", "freshest wins");
    }
}
