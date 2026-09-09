//! Fathom Video Notetaker — cloud meeting recorder with a public REST API. A
//! **Periodic** cloud pull of your recorded meetings + transcripts into the
//! [`crate::meetings`] contract. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/fathom.md.
//!
//! Three destinations, written in one pass:
//!
//! - **meetings contract** under `meetings/fathom/YYYY-MM.jsonl` (the
//!   [`crate::meetings`] contract): one [`Meeting`] per recording. Rows are
//!   **upserted by `guid`** into the month partition (read the month, merge by
//!   `guid` keeping the freshest, rewrite sorted) — so a re-poll never
//!   duplicates a meeting, and a transcript that arrives on a *later* poll
//!   updates the *same* row in place (the async-transcript path below).
//! - **raw meetings** under `meetings/fathom/raw/YYYY-MM.jsonl`: the verbatim
//!   API meeting objects at full fidelity (partitioned by the recording's start
//!   month, upserted by `recording_id`).
//! - **raw transcripts** under `meetings/fathom/raw/transcripts/<recording_id>.jsonl`:
//!   one per-meeting sidecar holding the full utterance stream. This is what
//!   the contract row's `transcript_ref` points at — transcripts are **sidecar
//!   artifacts, never inlined** into the contract row (the meetings convention).
//!
//! Auth is a per-user API key (a secret), pasted via the connection's
//! [`ConnectMethod::TokenPaste`] and stored under `.trove/sync/` (0600) like
//! the todoist token — it rides the `access_token` slot of a never-expiring
//! [`TokenSet`], is verified with a cheap `GET /meetings?limit=1` at connect
//! time, and never leaves the secret store (never logged, never in the cursor).
//!
//! 🔒 **Default-off opt-in.** Meeting transcripts are conversation content
//! (≈ message bodies), so the def ships `default_on: false`; the hub renders the
//! opt-in gate for default-off integrations.
//!
//! ## API — Fathom external v1 (`api.fathom.ai/external/v1`)
//!
//! Every call sends `X-Api-Key: <key>`. One endpoint does everything:
//!
//! - `GET /meetings?include_summary=true&include_highlights=true&include_transcript=true`
//!   → `{ "items": [...], "next_cursor": string|null }`. Per the API reference,
//!   for **API-key auth** the transcript, summary, and highlights are returned
//!   **embedded in each meeting object** when the matching `include_*` flag is
//!   set (`include_transcript`/`include_summary` are documented as "Unavailable
//!   for OAuth connected apps (use /recordings instead)" — that fallback is the
//!   OAuth path, not ours). There is **no** separate
//!   `/recordings/{id}/transcript` endpoint for this flow; the transcript is the
//!   meeting object's nullable `transcript` array
//!   (`[ { "speaker": { "display_name", "matched_calendar_invitee_email" },
//!   "text", "timestamp": "HH:MM:SS" } ]`). A freshly-ended meeting whose
//!   transcript hasn't processed yet simply has a `null`/empty `transcript`.
//!
//! **Cursor pagination — drain EVERY page, every poll** (follow `next_cursor`
//! until null). Fathom does not document the list's sort order, so a stop-early
//! traversal would strand meetings on later pages if the order is oldest-first
//! or unordered (the listenbrainz/BGG data-loss class). Meetings are
//! low-volume, so a full drain is cheap.
//!
//! ## Cursor / the async-transcript handling (simple, because transcripts embed)
//!
//! `.trove/fathom-sync.json` (non-secret, rebuildable) holds just
//! `last_meeting_ts` — the newest `recording_start_time` among meetings we've
//! stored **with a transcript**. It is a **write-filter, never a traversal
//! cutoff**: we always drain the full list, then (re)write a meeting only when
//! it is worth (re)writing — a *new* meeting (`start` newer than the watermark)
//! or any meeting still inside the recheck window (`start` within
//! [`RECHECK_DAYS`] of the watermark). The window is *not* conditioned on the
//! transcript state on purpose: a meeting that was incomplete and now has its
//! transcript sits *below* the watermark (a newer complete meeting advanced it
//! past this one), so re-writing recent meetings unconditionally picks up such
//! out-of-order transcript arrivals. Upsert-by-`guid` is idempotent, so
//! re-writing is harmless; the [`RECHECK_DAYS`] floor bounds the work (a
//! transcript that never arrives stops being re-checked once its meeting falls
//! outside the window — addressing the unbounded-recheck concern without any
//! per-id pending state).
//!
//! Because the transcript rides inside the meeting object, the async case is
//! trivial: on the poll where a meeting's embedded `transcript` first appears,
//! the same `guid` row is upserted in place with its `transcript_ref` set (and
//! the sidecar written) — exactly one row, never a duplicate, no separate
//! transcript fetch, no `pending` map, no row read-back.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::meetings::Meeting;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// The collector id and the source folder name.
const SOURCE: &str = "fathom";
/// Contract layer (one [`Meeting`] per recording, upserted by guid).
const CONTRACT_DIR: &str = "meetings/fathom";
/// Raw firehose (verbatim API meeting objects).
const RAW_DIR: &str = "meetings/fathom/raw";
/// Per-meeting transcript sidecars (`<recording_id>.jsonl`). `transcript_ref`
/// points here.
const TRANSCRIPT_DIR: &str = "meetings/fathom/raw/transcripts";

/// Non-secret rebuildable cursor (NOT under `.trove/sync/` — that's for 0600
/// secrets). Deleting it just re-pulls the window on the next sync.
const SYNC_FILE: &str = ".trove/fathom-sync.json";

/// The service id under `.trove/sync/` where the API key is stored (the
/// todoist/github slot: the key rides a never-expiring [`TokenSet`]).
const SERVICE: &str = "fathom";

const API_BASE: &str = "https://api.fathom.ai/external/v1";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs in the watcher loop. Every 15 min — well under the
/// 60/min budget for a personal account.
pub const FATHOM_SYNC_SECS: u64 = 900;

/// How far back of the watermark a still-incomplete meeting (embedded
/// transcript not yet present) keeps being re-checked for its transcript.
/// Transcripts process in minutes; 30 days is a generous safety margin that
/// also *bounds* the re-check work — a meeting that never gets a transcript
/// stops being re-written once it falls outside this window (so there is no
/// unbounded re-processing and no per-id pending state to grow).
const RECHECK_DAYS: i64 = 30;

/// The list query: one call returns metadata + embedded transcript + summary +
/// highlights for API-key auth.
const LIST_QUERY: &str =
    "include_summary=true&include_highlights=true&include_transcript=true";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_fathom_sync().last_meeting_ts.filter(|u| !u.is_empty())
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing key or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "fathom synced — {} meetings, {} transcripts",
                    c("meetings"),
                    c("transcripts"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "fathom sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "fathom",
        name: "Fathom",
        kind: IntegrationKind::CloudSync,
        // 🔒 Opt-in: meeting transcripts are conversation content (≈ message
        // bodies). The hub renders the acknowledgement gate for default-off.
        default_on: false,
        description: "Pulls your Fathom meeting recordings — transcripts with speaker labels, \
                      highlights, and summaries — into the unified meetings store via the \
                      official API (api.fathom.ai/external/v1), every 15 minutes. Uses a \
                      per-user API key; no paid plan required.",
        domain: "meetings",
        vault_path: "meetings/fathom/",
        toggleable: true,
        setup: &[
            "Connect with your Fathom API key on this card.",
            "Each sync pulls new recordings; transcripts attach to the same meeting once Fathom finishes processing them.",
        ],
        caveats: "Meeting transcripts are conversation content, so this source is off by \
                  default — turn it on deliberately. Transcripts are processed asynchronously: \
                  a just-ended meeting appears with its metadata first and gains its transcript \
                  on a later sync (the same row is updated, never duplicated).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(FATHOM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("fathom"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the X-Api-Key, a SECRET).

/// Verify the pasted key with a cheap `GET /meetings?limit=1`, then store it
/// (0600). A 401 bails with a clear message; the key is never logged.
fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key — paste your Fathom API key");
    }
    let client = FathomClient::new(API_BASE.to_string(), key.to_string());
    // A real call proves the key works and the account is reachable.
    match client.get_json("/meetings?limit=1") {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Fathom rejected the key (401) — check it's your API key from \
             Settings → API and hasn't been revoked"
        ),
        Err(e) => bail!("Fathom /meetings check failed: {e}"),
    }
    // The key goes ONLY through the secret store (0600). Never the cursor.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: key.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        },
    )
}

/// Forget the stored key. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = the key is stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Fathom".to_string(),
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // the API key doesn't expire
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: a personal key is self-service.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "fathom",
    display_name: "Fathom",
    methods: &[ConnectMethod::TokenPaste {
        label: "Fathom API key",
        help: "Fathom → Settings → API → create an API key.",
        placeholder: "fathom_api_key_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["fathom"],
    setup: &[
        "In Fathom, open Settings → API.",
        "Create an API key.",
        "Paste it here — it's stored locally (0600) and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors: 401 wants distinct handling (clear reconnect),
/// 429 is a transient rate-limit (we back off and the next tick retries),
/// everything else is a message. No error string ever carries the key.
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

/// The endpoints the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network.
trait FathomApi {
    /// `GET <path>` (path is API-relative, e.g. `/meetings?...`). Returns the
    /// parsed JSON body.
    fn get_json(&self, path: &str) -> Result<Value, FetchError>;
}

/// Thin client; base URL injected (the todoist/lastfm pattern).
struct FathomClient {
    base: String,
    key: String,
}

impl FathomClient {
    fn new(base: String, key: String) -> Self {
        FathomClient { base, key }
    }
}

impl FathomApi for FathomClient {
    fn get_json(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("X-Api-Key", &self.key)
            .set("Accept", "application/json")
            .call();
        match resp {
            Ok(resp) => {
                // Be polite: if the server reports the rate-limit window is
                // empty, pause until it resets before the next call.
                let reset = resp
                    .header("RateLimit-Remaining")
                    .and_then(|r| r.trim().parse::<u64>().ok())
                    .filter(|&rem| rem == 0)
                    .and_then(|_| {
                        resp.header("RateLimit-Reset").and_then(|s| s.trim().parse::<u64>().ok())
                    });
                if let Some(reset) = reset {
                    std::thread::sleep(Duration::from_secs(reset.min(60)));
                }
                resp.into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))
            }
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

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct SyncState {
    /// `recording_start_time` (RFC3339) of the newest meeting we've stored
    /// **with a transcript**. A write-filter watermark, never a traversal
    /// cutoff (we always drain the full list): a meeting is (re)written when its
    /// `start` is newer than this, or when it is within [`RECHECK_DAYS`] of it
    /// and still has no embedded transcript. Not a secret; rebuildable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_meeting_ts: Option<String>,
}

impl Vault {
    fn read_fathom_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_fathom_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (verbatim API meeting object, kept for the raw firehose).

/// One raw API meeting object in `meetings/fathom/raw/YYYY-MM.jsonl`. The
/// on-disk line is the verbatim API object (flattened — no synthetic keys
/// added, so a raw row round-trips byte-identically and stays full-fidelity).
/// `recording_id` (dedup key) and the start time (partition key) are read back
/// off the object via accessors, not stored as extra columns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawMeeting {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawMeeting {
    /// The recording id as a string (dedup key).
    fn guid(&self) -> String {
        self.fields.get("recording_id").and_then(value_id).unwrap_or_default()
    }

    /// The start timestamp (partition key): `recording_start_time`, falling
    /// back to `created_at`.
    fn start(&self) -> &str {
        self.fields
            .get("recording_start_time")
            .or_else(|| self.fields.get("created_at"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// An id field that may be a JSON number (Fathom returns integer
/// `recording_id`s) or a string → a `String`.
fn value_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Pull a string field, trimmed; `None` when missing/non-string/empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An RFC3339 UTC string → RFC3339 **local**. Unparseable values pass through
/// verbatim rather than being dropped (the todoist/github `to_local` idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Duration in seconds between two RFC3339 instants (end − start), `None` when
/// either is missing/unparseable or end precedes start.
fn duration_secs(start: &str, end: Option<&str>) -> Option<i64> {
    let end = end?;
    let s = DateTime::parse_from_rfc3339(start).ok()?;
    let e = DateTime::parse_from_rfc3339(end).ok()?;
    let secs = (e - s).num_seconds();
    (secs >= 0).then_some(secs)
}

/// A raw API meeting object → a normalized [`Meeting`] (without
/// `transcript_ref`; `summary` is filled here from `default_summary`). `None`
/// only when the object has no `recording_id` (can't dedup) or no usable start
/// timestamp (can't partition / place `ts`).
fn meeting_from_value(value: &Value) -> Option<Meeting> {
    let obj = value.as_object()?;
    let guid = obj.get("recording_id").and_then(value_id)?;

    // ts = recording_start_time, fall back created_at. Required to place the
    // row; without one we can't partition.
    let start_raw =
        str_opt(value, "recording_start_time").or_else(|| str_opt(value, "created_at"))?;
    let end_raw = str_opt(value, "recording_end_time");

    let mut m = Meeting::new(SOURCE, &guid, to_local(&start_raw));
    m.started = to_local(&start_raw);
    if let Some(end) = &end_raw {
        m.ended = to_local(end);
    }
    m.duration_secs = duration_secs(&start_raw, end_raw.as_deref());

    // title: meeting_title, fall back title.
    if let Some(title) = str_opt(value, "meeting_title").or_else(|| str_opt(value, "title")) {
        m.title = title;
    }

    // attendees from calendar_invitees[].email (lowercased); names ONLY when
    // fully aligned (every invitee has both an email and a name) — else the raw
    // invitees go to extra and no attendee_names is emitted (the contract rule).
    if let Some(invitees) = obj.get("calendar_invitees").and_then(Value::as_array) {
        let emails: Vec<String> = invitees
            .iter()
            .filter_map(|i| str_opt(i, "email").map(|e| e.to_lowercase()))
            .collect();
        let names: Vec<String> = invitees.iter().filter_map(|i| str_opt(i, "name")).collect();
        let aligned =
            !emails.is_empty() && emails.len() == invitees.len() && names.len() == emails.len();
        if !emails.is_empty() {
            m.attendees = emails;
        }
        if aligned {
            m.attendee_names = names;
        } else if !invitees.is_empty() {
            // Names misaligned (or partial) — preserve the raw invitees verbatim
            // rather than write a misaligned attendee_names.
            m.extra.insert("calendar_invitees".into(), Value::Array(invitees.clone()));
        }
    }

    // host = recorded_by.email (lowercased, same shaping as attendees).
    if let Some(host) = obj
        .get("recorded_by")
        .and_then(|r| str_opt(r, "email"))
        .map(|e| e.to_lowercase())
    {
        m.host = host;
    }

    // summary = default_summary.markdown_formatted (from include_summary).
    if let Some(summary) =
        obj.get("default_summary").and_then(|s| str_opt(s, "markdown_formatted"))
    {
        m.summary = summary;
    }

    // platform / meeting_url / recording_url where present.
    if let Some(platform) =
        str_opt(value, "meeting_platform").or_else(|| str_opt(value, "platform"))
    {
        m.platform = platform;
    }
    if let Some(url) = str_opt(value, "meeting_url") {
        m.meeting_url = url;
    }
    if let Some(url) = str_opt(value, "recording_url").or_else(|| str_opt(value, "url")) {
        m.recording_url = url;
    }

    // highlights[] → extra (timestamped clips, NOT the summary).
    if let Some(highlights) = obj.get("highlights").filter(|h| !h.is_null()) {
        let is_empty_arr = highlights.as_array().is_some_and(|a| a.is_empty());
        if !is_empty_arr {
            m.extra.insert("highlights".into(), highlights.clone());
        }
    }

    // A stable, useful key worth keeping on the contract row (the verbatim
    // object lives in the raw layer, so we don't bloat the row with the rest).
    if let Some(created) = str_opt(value, "created_at") {
        m.extra.insert("created_at".into(), Value::from(created));
    }

    Some(m)
}

// ---------------------------------------------------------------------------
// Transcript sidecar.

/// Vault-relative `transcript_ref` for a recording — a **per-meeting** sidecar
/// keyed by recording_id (NOT the month file), per the meetings convention.
fn transcript_ref(guid: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{guid}.jsonl")
}

/// The embedded transcript utterances of a meeting object: its nullable
/// `transcript` array. Empty/absent/null means Fathom hasn't finished
/// processing the transcript yet (the meeting is still "incomplete"). Also
/// tolerates a `{ "transcript": [...] }` wrapper or a bare array, for
/// robustness against shape drift.
fn transcript_utterances(v: &Value) -> Vec<Value> {
    match v {
        Value::Object(o) => o
            .get("transcript")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Upsert-by-guid into a month partition (the todoist raw idiom, applied to the
// CONTRACT rows too): read the target month, merge new rows by guid keeping the
// freshest, rewrite that partition sorted. A re-poll over an overlapping window
// never duplicates a guid, and a later transcript-bearing row REPLACES the
// earlier metadata-only row in place (same guid → same line).

/// Upsert contract [`Meeting`] rows by `guid` into their `ts`-month partitions.
/// Returns the count of rows that were new (not the count updated in place).
fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    upsert_partition(vault, CONTRACT_DIR, rows, |m| m.ts.clone(), |m| m.guid.clone())
}

/// Upsert raw [`RawMeeting`] objects by `recording_id` into their start-month
/// partitions. Returns the count of rows that were new.
fn upsert_raw(vault: &Vault, rows: Vec<RawMeeting>) -> Result<u64> {
    upsert_partition(vault, RAW_DIR, rows, |r| r.start().to_string(), |r| r.guid())
}

/// Shared upsert-into-month-partition: group by the month of `ts_of(row)`, and
/// within each month merge by `guid_of(row)` (freshest wins), rewriting the
/// partition sorted by (ts, guid). New-guid rows are counted.
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
            .with_context(|| format!("fathom: ts {ts:?} has no month (dir {dir})"))?
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
                Some(i) => existing[i] = r, // upsert in place — same guid, same line
                None => {
                    idx.insert(g, existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| ts_of(a).cmp(&ts_of(b)).then_with(|| guid_of(a).cmp(&guid_of(b))));
        vault.write_snapshot(&format!("{dir}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync. Missing key ⇒ a quiet skip on the periodic
/// path (mirror todoist/lastfm), a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let key = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|k| !k.trim().is_empty())
        .context("Fathom is not connected — add your API key in the Integrations tab")?;
    let client = FathomClient::new(API_BASE.to_string(), key);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl FathomApi) -> Result<PullOutcome> {
    let mut state = vault.read_fathom_sync();
    let watermark = state.last_meeting_ts.clone();
    // Re-check floor: a still-incomplete meeting newer than this is re-written
    // until its transcript appears; older incomplete ones are left as-is. On a
    // first sync (no watermark) the floor is unbounded — we write everything.
    let recheck_floor: Option<String> = watermark.as_deref().map(|w| {
        DateTime::parse_from_rfc3339(w)
            .map(|t| (t - chrono::Duration::days(RECHECK_DAYS)).to_rfc3339())
            .unwrap_or_else(|_| w.to_string())
    });

    // --- 1. drain EVERY page (sort order undocumented → never stop early) -
    // Follow next_cursor until null, collecting by recording_id (a meeting seen
    // on two pages is deduped). Meetings are low-volume, so a full drain is
    // cheap and immune to whatever order Fathom returns.
    let mut meetings: BTreeMap<String, Value> = BTreeMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/meetings?{LIST_QUERY}&cursor={}", urlencode(c)),
            None => format!("/meetings?{LIST_QUERY}"),
        };
        let body = api.get_json(&path).map_err(fetch_err)?;
        let (items, next_cursor) = parse_list(body);
        for item in items {
            if let Some(id) = item.get("recording_id").and_then(value_id) {
                meetings.insert(id, item);
            }
        }
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    // --- 2. per meeting: build the contract row + raw, with embedded transcript
    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut raw_rows: Vec<RawMeeting> = Vec::new();
    let mut transcripts_written = 0u64;
    let mut newest_with_transcript: Option<String> = watermark.clone();

    for (id, obj) in &meetings {
        let start = str_opt(obj, "recording_start_time")
            .or_else(|| str_opt(obj, "created_at"))
            .unwrap_or_default();
        let utterances = transcript_utterances(obj);
        let has_transcript = !utterances.is_empty();

        // Write-filter: a meeting is (re)written when it's a NEW meeting (start
        // newer than the watermark) or a RECENT one still inside the recheck
        // window (start within RECHECK_DAYS of the watermark). The recheck
        // window must NOT condition on the transcript state: a meeting that was
        // incomplete and *now* has its transcript sits below the watermark (a
        // newer complete meeting advanced it past this one), so we re-write it
        // to pick up the newly-arrived transcript — out-of-order completion.
        // Old meetings (outside the window) are skipped — already written on a
        // prior poll — which bounds the work (a transcript that never arrives
        // stops being re-checked once it falls outside the window). The first
        // sync (no watermark) writes everything.
        let is_new = watermark.as_deref().is_none_or(|w| start.as_str() > w);
        let recheck = recheck_floor.as_deref().is_some_and(|floor| start.as_str() > floor);
        if !is_new && !recheck {
            // An old complete meeting still advances the watermark high-water.
            if has_transcript && !start.is_empty() {
                newest_with_transcript = max_ts(newest_with_transcript, start);
            }
            continue;
        }

        // Raw firehose: the verbatim meeting object (embedded transcript and
        // all), full fidelity, upserted by recording_id.
        raw_rows.push(RawMeeting { fields: obj.as_object().cloned().unwrap_or_default() });

        let Some(mut row) = meeting_from_value(obj) else {
            // No usable id/start → can't place a contract row; the raw object is
            // still kept above.
            continue;
        };

        if has_transcript {
            // Transcript present (this poll or a prior one): write the sidecar
            // and point transcript_ref at it. The upsert-by-guid replaces any
            // earlier metadata-only row in place — same line, never a duplicate.
            write_transcript_sidecar(vault, id, &utterances)?;
            transcripts_written += 1;
            row.transcript_ref = transcript_ref(id);
            if !start.is_empty() {
                newest_with_transcript = max_ts(newest_with_transcript, start);
            }
        }
        contract_rows.push(row);
    }

    // --- 3. persist: raw firehose + contract upsert + cursor --------------
    let raw_new = upsert_raw(vault, raw_rows)?;
    let contract_new = upsert_contract(vault, contract_rows)?;

    state.last_meeting_ts = newest_with_transcript;
    vault.write_fathom_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("meetings", contract_new);
    counts.insert("transcripts", transcripts_written);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!(
            "Fathom synced — {contract_new} new meetings, {transcripts_written} transcripts"
        ),
        counts,
    })
}

/// The later of two RFC3339 starts (lexical compare is correct for the `…Z`
/// UTC form Fathom returns).
fn max_ts(cur: Option<String>, candidate: String) -> Option<String> {
    match cur {
        Some(prev) if prev.as_str() >= candidate.as_str() => Some(prev),
        _ => Some(candidate),
    }
}

/// Write the per-meeting transcript sidecar (full utterance fidelity), one
/// utterance per JSONL line, atomically. `timestamp` (an `HH:MM:SS` offset) and
/// every speaker field are stored verbatim.
fn write_transcript_sidecar(vault: &Vault, recording_id: &str, utterances: &[Value]) -> Result<()> {
    vault.write_snapshot(&transcript_ref(recording_id), utterances)
}

/// Pull `items` + `next_cursor` out of a Fathom list response. Tolerates a bare
/// array (a future shape change) as a single un-paged page.
fn parse_list(v: Value) -> (Vec<Value>, Option<String>) {
    match v {
        Value::Object(o) => {
            let items = o.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let next_cursor = o
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            (items, next_cursor)
        }
        Value::Array(a) => (a, None),
        _ => (Vec::new(), None),
    }
}

/// Map a [`FetchError`] at the top of the pull into an anyhow error with a
/// clear reconnect message for 401. No variant carries the key.
fn fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => {
            anyhow::anyhow!("Fathom rejected the key (401) — reconnect from the Integrations tab")
        }
        FetchError::RateLimited => {
            anyhow::anyhow!("Fathom rate limit hit (429) — will retry on the next sync")
        }
        other => anyhow::anyhow!("Fathom fetch failed: {other}"),
    }
}

/// Minimal percent-encoding for query/path values (the todoist/github idiom).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-fathom-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (the confirmed Fathom external-v1 shapes; transcript is
    // EMBEDDED in the meeting object per the API reference for API-key auth) --

    /// A two-utterance embedded transcript array, as the meeting object's
    /// nullable `transcript` field carries it.
    fn utterances(speaker: &str, email: &str) -> Value {
        serde_json::json!([
            {"speaker": {"display_name": speaker, "matched_calendar_invitee_email": email}, "text": "Hello there.", "timestamp": "00:00:01"},
            {"speaker": {"display_name": speaker, "matched_calendar_invitee_email": email}, "text": "Let's begin.", "timestamp": "00:00:05"}
        ])
    }

    /// A fully-recorded meeting object as `GET /meetings` returns it (with the
    /// `include_*` flags). `recording_id` is an INTEGER; the transcript rides
    /// **embedded** in the `transcript` field. (Fathom returns no `platform`
    /// field; it has `url`/`share_url`/`meeting_url`.)
    fn meeting_full(id: i64) -> Value {
        serde_json::json!({
            "recording_id": id,
            "meeting_title": "Q3 Roadmap Sync",
            "recording_start_time": "2026-06-10T16:00:00Z",
            "recording_end_time": "2026-06-10T16:49:00Z",
            "created_at": "2026-06-10T16:50:00Z",
            "url": "https://fathom.video/calls/123",
            "share_url": "https://fathom.video/share/abc",
            "meeting_url": "https://zoom.us/j/123456789",
            "calendar_invitees": [
                {"email": "DWills@Example.com", "name": "David Wills", "is_external": false},
                {"email": "Sam@Example.com", "name": "Sam Ortiz", "is_external": false}
            ],
            "recorded_by": {"email": "DWills@Example.com", "name": "David Wills", "team": "Eng"},
            "default_summary": {"template_name": "general", "markdown_formatted": "## Decisions\n- Ship the meetings contract first"},
            "highlights": [
                {"text": "key moment", "timestamp": "00:12:30"}
            ],
            "transcript": utterances("David Wills", "dwills@example.com")
        })
    }

    /// The same meeting but with its transcript still PROCESSING — the embedded
    /// `transcript` is `null` (Fathom hasn't produced it yet).
    fn meeting_no_transcript(id: i64) -> Value {
        let mut m = meeting_full(id);
        m["transcript"] = Value::Null;
        m
    }

    /// A just-ended meeting at a different time, transcript still processing
    /// (`transcript: null`).
    fn meeting_pending(id: i64) -> Value {
        serde_json::json!({
            "recording_id": id,
            "meeting_title": "Standup",
            "recording_start_time": "2026-06-11T09:00:00Z",
            "recording_end_time": "2026-06-11T09:15:00Z",
            "created_at": "2026-06-11T09:16:00Z",
            "calendar_invitees": [
                {"email": "DWills@Example.com", "name": "David Wills"}
            ],
            "recorded_by": {"email": "DWills@Example.com"},
            "transcript": Value::Null
        })
    }

    /// The pending meeting, now with its transcript embedded (a later poll).
    fn meeting_pending_with_transcript(id: i64) -> Value {
        let mut m = meeting_pending(id);
        m["transcript"] = utterances("David Wills", "dwills@example.com");
        m
    }

    /// A meeting whose invitees have emails but only PARTIAL names (so
    /// attendee_names must NOT be emitted — the misaligned invitees go to extra).
    fn meeting_partial_names(id: i64) -> Value {
        serde_json::json!({
            "recording_id": id,
            "meeting_title": "Customer Discovery",
            "recording_start_time": "2026-06-12T18:00:00Z",
            "recording_end_time": "2026-06-12T18:30:00Z",
            "calendar_invitees": [
                {"email": "jordan@acme.com", "name": "Jordan"},
                {"email": "dwills@example.com"}
            ],
            "recorded_by": {"email": "dwills@example.com"},
            "transcript": utterances("Jordan", "jordan@acme.com")
        })
    }

    // --- a scripted mock API (ONE endpoint: GET /meetings, cursor-paginated) -

    /// Maps a `/meetings` cursor value to its list page. The first (cursorless)
    /// request gets the page registered with cursor `None`; a `?cursor=<v>`
    /// request gets the page registered for `<v>`. There is no transcript
    /// endpoint — transcripts ride embedded in the meeting objects.
    struct MockApi {
        /// (cursor-in-request → page body). `None` cursor = the first page.
        pages: RefCell<Vec<(Option<String>, Value)>>,
        unauthorized: RefCell<HashSet<String>>,
        requests: RefCell<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                unauthorized: RefCell::new(HashSet::new()),
                requests: RefCell::new(Vec::new()),
            }
        }

        /// Register the first (cursorless) `/meetings` page.
        fn meetings_page(&self, items: Vec<Value>, next_cursor: Option<&str>) {
            self.page(None, items, next_cursor);
        }

        /// Register the page returned for a given `?cursor=<cursor>` request.
        fn page(&self, cursor: Option<&str>, items: Vec<Value>, next_cursor: Option<&str>) {
            let body = serde_json::json!({ "items": items, "next_cursor": next_cursor });
            self.pages.borrow_mut().push((cursor.map(str::to_string), body));
        }

        fn requested(&self, needle: &str) -> bool {
            self.requests.borrow().iter().any(|p| p.contains(needle))
        }

        /// The `cursor=<v>` value in a request path, if any.
        fn cursor_in(path: &str) -> Option<String> {
            path.split("cursor=").nth(1).map(|rest| {
                rest.split('&').next().unwrap_or("").to_string()
            })
        }
    }

    impl FathomApi for MockApi {
        fn get_json(&self, path: &str) -> Result<Value, FetchError> {
            self.requests.borrow_mut().push(path.to_string());
            if self.unauthorized.borrow().iter().any(|p| path.contains(p)) {
                return Err(FetchError::Unauthorized);
            }
            let want = Self::cursor_in(path);
            for (cursor, body) in self.pages.borrow().iter() {
                if *cursor == want {
                    return Ok(body.clone());
                }
            }
            // An unregistered cursor (or no first page) → an empty terminal page.
            Ok(serde_json::json!({"items": [], "next_cursor": null}))
        }
    }

    // --- pure mapping tests ---------------------------------------------

    #[test]
    fn maps_meeting_guid_ts_duration_attendees_summary_highlights() {
        let m = meeting_from_value(&meeting_full(123)).unwrap();
        assert_eq!(m.source, "fathom");
        assert_eq!(m.guid, "123", "guid is recording_id as a string");
        // ts = start, converted to local (same instant as the UTC).
        assert_eq!(
            DateTime::parse_from_rfc3339(&m.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T16:00:00Z").unwrap().timestamp(),
        );
        assert_eq!(m.title, "Q3 Roadmap Sync");
        // duration computed from end − start = 49 min = 2940 s.
        assert_eq!(m.duration_secs, Some(2940));
        // attendees lowercased; names aligned (both invitees had a name).
        assert_eq!(m.attendees, vec!["dwills@example.com", "sam@example.com"]);
        assert_eq!(m.attendee_names, vec!["David Wills", "Sam Ortiz"]);
        assert_eq!(m.host, "dwills@example.com", "host lowercased from recorded_by");
        assert!(m.summary.contains("Ship the meetings contract first"));
        assert_eq!(m.meeting_url, "https://zoom.us/j/123456789");
        assert_eq!(m.recording_url, "https://fathom.video/calls/123", "recording_url from url");
        // highlights → extra (NOT the summary). The embedded transcript is NOT
        // copied into the contract row — it's a sidecar (set by the pull loop).
        assert!(m.extra.contains_key("highlights"));
        assert_eq!(m.transcript_ref, "", "mapping is metadata-only; pull sets transcript_ref");
    }

    #[test]
    fn partial_names_drop_to_extra_and_no_attendee_names() {
        let m = meeting_from_value(&meeting_partial_names(7)).unwrap();
        // Emails still captured (lowercased).
        assert_eq!(m.attendees, vec!["jordan@acme.com", "dwills@example.com"]);
        // Names misaligned (only one of two) → NO attendee_names; raw invitees
        // preserved in extra.
        assert!(m.attendee_names.is_empty(), "misaligned names dropped, not written");
        assert!(m.extra.contains_key("calendar_invitees"));
    }

    #[test]
    fn duration_omitted_when_end_missing_or_inverted() {
        assert_eq!(
            duration_secs("2026-06-10T16:00:00Z", Some("2026-06-10T16:49:00Z")),
            Some(2940)
        );
        assert_eq!(duration_secs("2026-06-10T16:00:00Z", None), None, "no end → none");
        assert_eq!(
            duration_secs("2026-06-10T16:00:00Z", Some("2026-06-10T15:00:00Z")),
            None,
            "end before start → none"
        );
    }

    #[test]
    fn transcript_ref_is_a_per_meeting_sidecar_path() {
        assert_eq!(transcript_ref("123"), "meetings/fathom/raw/transcripts/123.jsonl");
    }

    #[test]
    fn embedded_transcript_extracted_null_is_empty() {
        // The embedded `transcript` array is read off the meeting object; a null
        // transcript yields no utterances (still processing).
        assert_eq!(transcript_utterances(&meeting_full(1)).len(), 2);
        assert!(transcript_utterances(&meeting_no_transcript(1)).is_empty());
        assert!(transcript_utterances(&meeting_pending(1)).is_empty());
    }

    #[test]
    fn list_query_requests_embedded_transcript() {
        // Guard the fix: the list call asks for the embedded transcript (+
        // summary + highlights), not a separate endpoint.
        assert!(LIST_QUERY.contains("include_transcript=true"));
        assert!(LIST_QUERY.contains("include_summary=true"));
        assert!(LIST_QUERY.contains("include_highlights=true"));
    }

    #[test]
    fn parse_list_reads_items_cursor_and_bare_array() {
        let (items, cur) = parse_list(serde_json::json!({"items": [1, 2], "next_cursor": "c1"}));
        assert_eq!(items.len(), 2);
        assert_eq!(cur.as_deref(), Some("c1"));
        let (items, cur) = parse_list(serde_json::json!({"items": [1], "next_cursor": null}));
        assert_eq!(items.len(), 1);
        assert_eq!(cur, None);
        let (items, cur) = parse_list(serde_json::json!([1, 2, 3]));
        assert_eq!(items.len(), 3);
        assert_eq!(cur, None);
    }

    // --- full pull + dual write -----------------------------------------

    #[test]
    fn full_pull_writes_contract_raw_transcript_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new();
        api.meetings_page(vec![meeting_full(123)], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1), "one new contract row");
        assert_eq!(out.counts.get("transcripts"), Some(&1));
        assert_eq!(out.counts.get("raw"), Some(&1));
        // No separate transcript endpoint is ever called — only /meetings.
        assert!(api.requested("/meetings"));
        assert!(!api.requested("/recordings/"), "no separate transcript endpoint");

        // Contract row in the ts-month partition (June, local).
        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.guid, "123");
        assert_eq!(m.transcript_ref, "meetings/fathom/raw/transcripts/123.jsonl");

        // Sidecar written with the full utterance stream, timestamps verbatim.
        let sidecar = v.root().join("meetings/fathom/raw/transcripts/123.jsonl");
        assert!(sidecar.exists());
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(body.lines().count(), 2, "two utterances, one per line");
        assert!(body.contains("\"timestamp\":\"00:00:01\""), "HH:MM:SS offset verbatim");
        assert!(body.contains("matched_calendar_invitee_email"));

        // Raw firehose: verbatim meeting object, full fidelity (embedded transcript and all).
        let raw = std::fs::read_to_string(v.root().join("meetings/fathom/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("\"recording_id\":123"), "raw keeps the integer id verbatim");
        assert!(raw.contains("default_summary"));

        // Watermark advanced to the meeting's start; no key in the cursor.
        let state = v.read_fathom_sync();
        assert_eq!(state.last_meeting_ts.as_deref(), Some("2026-06-10T16:00:00Z"));
    }

    // --- THE async-transcript path: one row, no duplicate ---------------

    #[test]
    fn async_transcript_upserts_same_guid_row_no_duplicate() {
        let v = temp_vault("async");

        // Poll 1: the meeting is listed but its embedded transcript is null
        // (still processing).
        let api1 = MockApi::new();
        api1.meetings_page(vec![meeting_pending(555)], None);
        let out1 = pull_with(&v, &api1).unwrap();
        assert_eq!(out1.counts.get("transcripts"), Some(&0), "no transcript yet");

        // The metadata-only row is written WITHOUT transcript_ref.
        let key = Partition::Month.key(&to_local("2026-06-11T09:00:00Z")).unwrap().to_string();
        let rows1: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows1.len(), 1, "exactly one row");
        assert_eq!(rows1[0].guid, "555");
        assert_eq!(rows1[0].transcript_ref, "", "metadata-only: no transcript_ref");
        assert_eq!(rows1[0].title, "Standup");
        // Watermark NOT advanced past the still-incomplete meeting (no transcript).
        let st1 = v.read_fathom_sync();
        assert!(st1.last_meeting_ts.is_none(), "watermark not advanced past an incomplete meeting");

        // Poll 2: the SAME meeting now lists with its embedded transcript. The
        // same guid row is upserted in place — exactly one row, now with
        // transcript_ref. (Full-drain re-sees it; the recheck window also covers
        // it even if the watermark had moved.)
        let api2 = MockApi::new();
        api2.meetings_page(vec![meeting_pending_with_transcript(555)], None);
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("transcripts"), Some(&1), "transcript now present");

        let rows2: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows2.len(), 1, "STILL exactly one row — upsert, not duplicate");
        assert_eq!(rows2[0].guid, "555");
        assert_eq!(
            rows2[0].transcript_ref, "meetings/fathom/raw/transcripts/555.jsonl",
            "the same row now carries transcript_ref"
        );
        // Sidecar now exists; watermark advanced.
        assert!(v.root().join("meetings/fathom/raw/transcripts/555.jsonl").exists());
        let st2 = v.read_fathom_sync();
        assert_eq!(st2.last_meeting_ts.as_deref(), Some("2026-06-11T09:00:00Z"));
    }

    #[test]
    fn recent_incomplete_meeting_rechecked_even_below_watermark() {
        // Out-of-order completion: a newer meeting (with a transcript) advances
        // the watermark past an older still-incomplete one; the older one must
        // STILL be re-checked (within RECHECK_DAYS) and gain its transcript_ref
        // on a later poll — never stranded.
        let v = temp_vault("recheck");
        // Poll 1: older meeting 100 (start 06-11, no transcript) + newer meeting
        // 200 (start 06-12, has transcript).
        let api1 = MockApi::new();
        api1.meetings_page(
            vec![meeting_pending(100), {
                let mut m = meeting_full(200);
                m["recording_start_time"] = serde_json::json!("2026-06-12T16:00:00Z");
                m["recording_end_time"] = serde_json::json!("2026-06-12T16:30:00Z");
                m
            }],
            None,
        );
        pull_with(&v, &api1).unwrap();
        // Watermark is at the NEWER complete meeting (06-12), past the incomplete one.
        let st1 = v.read_fathom_sync();
        assert_eq!(st1.last_meeting_ts.as_deref(), Some("2026-06-12T16:00:00Z"));

        // Poll 2: meeting 100 now has its transcript (start 06-11, BELOW the
        // watermark). The recheck window must still pick it up.
        let api2 = MockApi::new();
        api2.meetings_page(vec![meeting_pending_with_transcript(100)], None);
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("transcripts"), Some(&1), "the below-watermark incomplete meeting was rechecked");

        let key = Partition::Month.key(&to_local("2026-06-11T09:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        let m100 = rows.iter().find(|m| m.guid == "100").unwrap();
        assert_eq!(m100.transcript_ref, "meetings/fathom/raw/transcripts/100.jsonl");
    }

    // --- cursor pagination drains ALL pages -----------------------------

    #[test]
    fn cursor_pagination_drains_all_pages() {
        let v = temp_vault("paginate");
        let api = MockApi::new();
        // Three pages chained by cursor; meetings spread across them so a
        // stop-early traversal would lose page 2/3.
        api.page(None, vec![meeting_full(1)], Some("CUR2"));
        api.page(Some("CUR2"), vec![meeting_full(2)], Some("CUR3"));
        api.page(Some("CUR3"), vec![meeting_full(3)], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&3), "all three pages' meetings landed");
        assert!(api.requested("cursor=CUR2"), "page 2 fetched");
        assert!(api.requested("cursor=CUR3"), "page 3 fetched");

        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        let guids: Vec<&str> = rows.iter().map(|r| r.guid.as_str()).collect();
        assert!(guids.contains(&"1") && guids.contains(&"2") && guids.contains(&"3"));
    }

    #[test]
    fn drains_all_pages_even_when_first_page_is_old() {
        // The data-loss guard: even if page 1 holds only OLD meetings (well
        // outside the recheck window, so they'd be skipped) and the NEW meeting
        // sits on page 2, the full drain still reaches and collects it (sort
        // order is undocumented → never stop early). A stop-early traversal
        // keyed on page 1 would lose meeting 2 forever.
        let v = temp_vault("drain-old-first");
        v.write_fathom_sync(&SyncState { last_meeting_ts: Some("2026-06-10T16:00:00Z".into()) }).unwrap();
        let api = MockApi::new();
        // Page 1: an ancient meeting (January — outside the 30-day window).
        api.page(None, vec![{
            let mut m = meeting_full(1);
            m["recording_start_time"] = serde_json::json!("2026-01-05T16:00:00Z");
            m["recording_end_time"] = serde_json::json!("2026-01-05T16:30:00Z");
            m
        }], Some("P2"));
        // Page 2: a newer meeting (past the watermark).
        api.page(Some("P2"), vec![{
            let mut m = meeting_full(2);
            m["recording_start_time"] = serde_json::json!("2026-06-20T16:00:00Z");
            m["recording_end_time"] = serde_json::json!("2026-06-20T16:30:00Z");
            m
        }], None);

        let out = pull_with(&v, &api).unwrap();
        assert!(api.requested("cursor=P2"), "page 2 fetched despite an all-old page 1");
        assert_eq!(out.counts.get("meetings"), Some(&1), "only the newer page-2 meeting written (old one skipped)");
        let key = Partition::Month.key(&to_local("2026-06-20T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert!(rows.iter().any(|m| m.guid == "2"));
    }

    // --- watermark as a write-filter ------------------------------------

    #[test]
    fn watermark_skips_complete_meetings_outside_recheck_window() {
        let v = temp_vault("watermark");
        // Watermark in March; a complete meeting from January is far outside the
        // 30-day recheck window → not re-written (the write-filter bound).
        v.write_fathom_sync(&SyncState { last_meeting_ts: Some("2026-03-10T16:00:00Z".into()) }).unwrap();
        let api = MockApi::new();
        let mut old = meeting_full(123);
        old["recording_start_time"] = serde_json::json!("2026-01-05T16:00:00Z");
        old["recording_end_time"] = serde_json::json!("2026-01-05T16:30:00Z");
        api.meetings_page(vec![old], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&0), "old complete meeting outside the window skipped");
        let key = Partition::Month.key(&to_local("2026-01-05T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert!(rows.is_empty(), "the out-of-window meeting was not re-written");
    }

    #[test]
    fn at_watermark_rewrite_is_idempotent_no_duplicate() {
        // A complete meeting AT the watermark (so inside the recheck window) is
        // re-written every poll — harmless: upsert-by-guid keeps exactly ONE row.
        let v = temp_vault("at-watermark");
        let api1 = MockApi::new();
        api1.meetings_page(vec![meeting_full(123)], None);
        pull_with(&v, &api1).unwrap(); // watermark now at 123's start
        let api2 = MockApi::new();
        api2.meetings_page(vec![meeting_full(123)], None);
        pull_with(&v, &api2).unwrap(); // 123 re-seen, within the window, re-written
        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1, "re-write within the window never duplicates");
    }

    #[test]
    fn newer_meeting_past_watermark_is_collected() {
        let v = temp_vault("watermark-new");
        v.write_fathom_sync(&SyncState { last_meeting_ts: Some("2026-06-09T00:00:00Z".into()) }).unwrap();
        let api = MockApi::new();
        api.meetings_page(vec![meeting_full(123)], None); // start 2026-06-10 > watermark
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1), "the newer meeting is collected");
    }

    #[test]
    fn ancient_incomplete_meeting_not_rechecked_beyond_window() {
        // The #3 bound: a meeting far older than RECHECK_DAYS that still has no
        // transcript is NOT re-written (no unbounded re-processing).
        let v = temp_vault("ancient");
        // Watermark is recent (June); an incomplete meeting from January is way
        // outside the 30-day recheck window.
        v.write_fathom_sync(&SyncState { last_meeting_ts: Some("2026-06-10T16:00:00Z".into()) }).unwrap();
        let api = MockApi::new();
        let mut ancient = meeting_pending(999);
        ancient["recording_start_time"] = serde_json::json!("2026-01-01T10:00:00Z");
        ancient["recording_end_time"] = serde_json::json!("2026-01-01T10:30:00Z");
        api.meetings_page(vec![ancient], None);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&0), "ancient incomplete meeting not rechecked");
    }

    // --- re-poll dedupe (no duplicate when transcript already present) --

    #[test]
    fn resync_same_meeting_no_duplicate_row() {
        let v = temp_vault("resync");
        let api1 = MockApi::new();
        api1.meetings_page(vec![meeting_full(123)], None);
        pull_with(&v, &api1).unwrap();

        // Re-poll the same meeting; clear the watermark so it's "new" again.
        // The upsert must still keep ONE row (and one raw line).
        v.write_fathom_sync(&SyncState::default()).unwrap();
        let api2 = MockApi::new();
        api2.meetings_page(vec![meeting_full(123)], None);
        pull_with(&v, &api2).unwrap();

        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1, "upsert by guid — one row after a re-poll");
        let raw = std::fs::read_to_string(v.root().join("meetings/fathom/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "raw deduped by recording_id too");
    }

    // --- token never logged / never in the cursor ----------------------

    #[test]
    fn key_never_in_cursor_and_stored_0600() {
        let v = temp_vault("secret");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "fathom_secret_xyz".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Fathom");
        assert_eq!(status.accounts[0].key, "fathom");

        // Run a pull (offline mock) and confirm the cursor carries no key.
        let api = MockApi::new();
        api.meetings_page(vec![meeting_full(9)], None);
        pull_with(&v, &api).unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/fathom-sync.json")).unwrap();
        assert!(!cursor.contains("fathom_secret_xyz"), "API key never in the cursor");
        assert!(!cursor.contains("access_token"), "no token field in the cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("fathom_secret_xyz") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret key file must be 0600");
                }
            }
            assert!(found, "the key was stored under .trove/sync");
        }

        def_disconnect(&v, "fathom").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
    }

    #[test]
    fn empty_key_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn unauthorized_surfaces_clear_reconnect_message() {
        let v = temp_vault("401");
        let api = MockApi::new();
        api.unauthorized.borrow_mut().insert("/meetings".to_string());
        let err = pull_with(&v, &api).unwrap_err().to_string();
        assert!(err.contains("401"), "401 surfaced: {err}");
        assert!(!err.contains("secret"), "no secret leaks into the error");
    }

    // --- serde back-compat ----------------------------------------------

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_meeting_ts.is_none());
        // A cursor with only last_meeting_ts still deserializes.
        let partial: SyncState =
            serde_json::from_str(r#"{"last_meeting_ts":"2026-06-01T00:00:00Z"}"#).unwrap();
        assert_eq!(partial.last_meeting_ts.as_deref(), Some("2026-06-01T00:00:00Z"));
        // A legacy cursor that still carries the now-removed `pending` field is
        // tolerated (unknown field ignored) — no crash, watermark preserved.
        let legacy: SyncState = serde_json::from_str(
            r#"{"last_meeting_ts":"2026-06-01T00:00:00Z","pending":{"555":"2026-06-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(legacy.last_meeting_ts.as_deref(), Some("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn meeting_serde_back_compat_old_line_still_deserializes() {
        // A row written by an older/sparser writer (only the core + a couple
        // fields, an unknown extra-shaped top-level key) must still deserialize
        // and round-trip. Guards the additive-evolution promise.
        let old = serde_json::json!({
            "ts": "2026-06-02T11:05:00-07:00",
            "source": "fathom",
            "guid": "42",
            "title": "Old Meeting",
            "future_field": "ignored"
        });
        let m: Meeting = serde_json::from_value(old).unwrap();
        assert_eq!(m.guid, "42");
        assert_eq!(m.title, "Old Meeting");
        assert!(m.transcript_ref.is_empty());
        assert!(m.attendees.is_empty());
        // Re-serialize: omit-empty keeps it lean, the unknown field is gone.
        let re = serde_json::to_value(&m).unwrap();
        assert!(re.get("future_field").is_none());
        assert!(re.get("attendees").is_none(), "empty vec omitted");
    }

    #[test]
    fn raw_meeting_roundtrips_full_fidelity() {
        let r = RawMeeting { fields: meeting_full(123).as_object().unwrap().clone() };
        assert_eq!(r.guid(), "123");
        assert_eq!(r.start(), "2026-06-10T16:00:00Z");
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("\"guid\""), "no synthetic guid column on disk");
        let back: RawMeeting = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r, "round-trips identically");
        assert!(line.contains("\"recording_id\":123"));
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "fathom");
    }
}
