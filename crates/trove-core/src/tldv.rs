//! tl;dv — AI meeting recorder covering Zoom, Google Meet, and Teams.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/tldv.md.
//!
//! Three destinations, written in one pass:
//!
//! - **meetings contract** under `meetings/tldv/YYYY-MM.jsonl` (the
//!   [`crate::meetings`] contract): one [`Meeting`] per recording. Rows are
//!   **upserted by `guid`** into the month partition (read the month, merge by
//!   `guid` keeping the freshest, rewrite sorted) so a re-poll never duplicates
//!   a meeting.
//! - **raw meetings** under `meetings/tldv/raw/YYYY-MM.jsonl`: the verbatim API
//!   meeting objects at full fidelity (partitioned by the recording's start month,
//!   upserted by `id`).
//! - **transcript sidecars** under
//!   `meetings/tldv/raw/transcripts/<meetingId>.jsonl`: one per-meeting sidecar
//!   holding the full utterance stream from `GET /v1alpha1/meetings/{id}/transcript`.
//!   [`Meeting::transcript_ref`] points here when a transcript is present.
//!
//! ## API — tl;dv REST API v1alpha1 (`https://pasta.tldv.io`)
//!
//! Every call sends `x-api-key: <key>`. Endpoints used:
//!
//! - `GET /v1alpha1/meetings?page=<n>` — paginated list (page-based: `page`,
//!   `pages`, `total`, `pageSize`, `results[]`). Meeting objects contain:
//!   `id`, `name`, `happenedAt` (ISO timestamp), `url` (recording URL),
//!   `duration` (seconds as number), `organizer` (obj: `name`, `email`),
//!   `invitees` (array of obj: `name`, `email`), `extraProperties.conferenceId`.
//! - `GET /v1alpha1/meetings/{meetingId}/transcript` — returns
//!   `{ id, meetingId, data: [ {speaker, text, startTime, endTime} ] }`.
//!   Called for each new meeting to obtain the speaker-labeled utterances.
//! - `GET /v1alpha1/meetings/{meetingId}/notes` — returns
//!   `{ structuredNotes, markdownContent, topics[] }`. `markdownContent` feeds
//!   `Meeting::summary`; full response goes to raw + `extra`.
//!
//! **Pagination**: drain all pages in order (page 1, 2, …) until `page >= pages`.
//! The API uses page numbers (not cursors). Meetings are low-volume; a full
//! drain on each poll is cheap and safe.
//!
//! ## Cursor
//!
//! `.trove/tldv-sync.json` (non-secret, rebuildable) holds `last_happened_at`
//! — the newest `happenedAt` among meetings stored. It is a write-filter
//! watermark: on each poll we drain ALL pages but only (re)write meetings whose
//! `happenedAt` is newer than the watermark OR within the [`RECHECK_DAYS`]
//! recheck window.
//!
//! tl;dv transcribes asynchronously: a just-ended meeting may be listed before
//! its transcript is ready. Without a recheck window the first poll would write
//! the row with no `transcript_ref`, advance the watermark, and subsequent polls
//! would skip the meeting forever. The recheck window ensures any meeting whose
//! `happenedAt` is within [`RECHECK_DAYS`] of the watermark is re-processed on
//! every poll — the guid-upsert is idempotent, so this is free — until its
//! transcript lands and `transcript_ref` is set. A meeting older than the window
//! stops being re-checked, bounding the work.
//!
//! The watermark advances only AFTER a full successful drain. A guid/parse miss
//! does not advance it.
//!
//! ## Plan gating
//!
//! API access requires Pro or Business plan. The connect card states this
//! plainly (the disabled-controls-affordance rule). Free-tier users see the
//! card in a gated/locked state with a plan-upgrade hint.
//!
//! 🔒 **Default-off opt-in.** Meeting transcripts are conversation content
//! (≈ message bodies), so the def ships `default_on: false`.

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

/// The collector id and source folder name.
const SOURCE: &str = "tldv";
/// Contract layer (one [`Meeting`] per recording, upserted by guid).
const CONTRACT_DIR: &str = "meetings/tldv";
/// Raw firehose (verbatim API meeting objects).
const RAW_DIR: &str = "meetings/tldv/raw";
/// Per-meeting transcript sidecars (`<meetingId>.jsonl`).
const TRANSCRIPT_DIR: &str = "meetings/tldv/raw/transcripts";

/// Non-secret, rebuildable cursor file.
const SYNC_FILE: &str = ".trove/tldv-sync.json";
/// The service id under `.trove/sync/` where the API key is stored.
const SERVICE: &str = "tldv";

const API_BASE: &str = "https://pasta.tldv.io";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Sync every 30 minutes — meetings are low-volume, transcripts process in
/// minutes; consistent with Granola's cadence for the same tier of service.
pub const TLDV_SYNC_SECS: u64 = 1_800;

/// Results per page for `GET /v1alpha1/meetings`.
const PAGE_SIZE: u32 = 50;

/// How far back of the watermark a meeting keeps being re-checked for its
/// transcript. tl;dv transcribes asynchronously; a just-ended meeting is often
/// listed before its transcript is ready. 30 days is a generous safety margin
/// that also bounds the recheck work — a transcript that never arrives stops
/// being re-checked once the meeting falls outside this window.
const RECHECK_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_tldv_sync().last_happened_at.filter(|s| !s.is_empty())
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "tl;dv synced — {} meetings, {} transcripts",
                    c("meetings"),
                    c("transcripts"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "tl;dv sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tldv",
        name: "tl;dv",
        kind: IntegrationKind::CloudSync,
        // 🔒 Opt-in: meeting transcripts are conversation content (≈ message bodies).
        default_on: false,
        description: "Pulls your tl;dv meeting recordings — attendees, AI notes, and \
                      speaker-labeled transcripts — into the vault via the official REST API \
                      (pasta.tldv.io), every 30 minutes. Requires the Pro or Business plan.",
        domain: "meetings",
        vault_path: "meetings/tldv/",
        toggleable: true,
        setup: &[
            "Connect with your tl;dv API key on this card.",
            "Each sync pulls new recordings; transcripts and notes attach to the same meeting row.",
        ],
        caveats: "API access requires the tl;dv Pro or Business plan — the free tier has no \
                  programmatic export. Meeting transcripts are conversation content, so this \
                  source is off by default — turn it on deliberately.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(TLDV_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("tldv"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the x-api-key, a SECRET).

/// Verify the pasted key with a cheap `GET /v1alpha1/meetings?pageSize=1`.
fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key — paste your tl;dv API key");
    }
    let client = TldvClient::new(API_BASE.to_string(), key.to_string());
    match client.list_meetings(1, 1) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "tl;dv rejected the key (401) — check it's your API key from \
             Settings → API and hasn't been revoked. API access requires the Pro or Business plan."
        ),
        Err(FetchError::Forbidden) => bail!(
            "tl;dv returned 403 — API access requires the Pro or Business plan. \
             Upgrade your plan and try again."
        ),
        Err(e) => bail!("tl;dv /meetings check failed: {e}"),
    }
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

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "tl;dv".to_string(),
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
    id: "tldv",
    display_name: "tl;dv",
    methods: &[ConnectMethod::TokenPaste {
        label: "tl;dv API key",
        help: "tl;dv → Settings → API → create an API key (Pro or Business plan required).",
        placeholder: "tldv_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["tldv"],
    setup: &[
        "In tl;dv, open Settings → API.",
        "Create an API key (requires Pro or Business plan).",
        "Paste it here — it's stored locally (0600) and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — trait-injectable so tests run fully offline.

/// Status-level fetch errors. No variant carries the key.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Forbidden,
    NotFound,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Forbidden => write!(f, "forbidden (HTTP 403 — plan upgrade required)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The API surface the pull needs. A trait so tests drive logic offline.
trait TldvApi {
    /// `GET /v1alpha1/meetings?pageSize=<n>&page=<p>`
    fn list_meetings(&self, page_size: u32, page: u32) -> Result<Value, FetchError>;

    /// `GET /v1alpha1/meetings/{id}/transcript`
    fn get_transcript(&self, id: &str) -> Result<Value, FetchError>;

    /// `GET /v1alpha1/meetings/{id}/notes`
    fn get_notes(&self, id: &str) -> Result<Value, FetchError>;
}

/// Thin live client; base URL injected for testability.
struct TldvClient {
    base: String,
    key: String,
}

impl TldvClient {
    fn new(base: String, key: String) -> Self {
        TldvClient { base, key }
    }

    fn get_json(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("x-api-key", &self.key)
            .set("Accept", "application/json")
            .call();
        match resp {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
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

impl TldvApi for TldvClient {
    fn list_meetings(&self, page_size: u32, page: u32) -> Result<Value, FetchError> {
        self.get_json(&format!("/v1alpha1/meetings?pageSize={page_size}&page={page}"))
    }

    fn get_transcript(&self, id: &str) -> Result<Value, FetchError> {
        self.get_json(&format!("/v1alpha1/meetings/{id}/transcript"))
    }

    fn get_notes(&self, id: &str) -> Result<Value, FetchError> {
        self.get_json(&format!("/v1alpha1/meetings/{id}/notes"))
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct SyncState {
    /// `happenedAt` (RFC3339) of the newest meeting we've stored. A write-filter
    /// watermark — we drain ALL pages but only write meetings newer than this.
    /// Advances only after a full successful drain. Not a secret; rebuildable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_happened_at: Option<String>,
}

impl Vault {
    fn read_tldv_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_tldv_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row (verbatim API meeting object from the list endpoint, enriched with
// transcript + notes at fetch time).

/// One raw API meeting object in `meetings/tldv/raw/YYYY-MM.jsonl`. Stored
/// verbatim (flattened) — no synthetic keys added.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawMeeting {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawMeeting {
    fn id(&self) -> String {
        self.fields.get("id").and_then(Value::as_str).unwrap_or("").to_string()
    }

    /// `happenedAt` (ISO8601) — the partition key timestamp.
    fn happened_at(&self) -> &str {
        self.fields.get("happenedAt").and_then(Value::as_str).unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Pure mapping helpers (fixture-tested).

/// Pull a string field, trimmed, non-empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An RFC3339 / ISO8601 UTC string → RFC3339 local. Unparseable values pass
/// through verbatim (the fathom/granola `to_local` idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Vault-relative `transcript_ref` for a meeting — per-meeting sidecar.
fn transcript_ref(meeting_id: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{meeting_id}.jsonl")
}

/// Map a tl;dv API meeting object → a [`Meeting`] contract row.
/// `notes_markdown` and `transcript_ready` are provided by the pull after
/// fetching the per-meeting detail endpoints.
///
/// Returns `None` only when `id` is absent (can't dedup) or `happenedAt`
/// is absent (can't partition).
fn meeting_from_value(
    value: &Value,
    notes_markdown: Option<&str>,
    extra_fields: Map<String, Value>,
) -> Option<Meeting> {
    let obj = value.as_object()?;
    let guid = str_opt(value, "id")?;
    let happened_at = str_opt(value, "happenedAt")?;

    let mut m = Meeting::new(SOURCE, &guid, to_local(&happened_at));
    m.started = to_local(&happened_at);

    // title = name.
    if let Some(name) = str_opt(value, "name") {
        m.title = name;
    }

    // duration: the API returns duration in seconds as a number.
    if let Some(secs) = obj.get("duration").and_then(Value::as_f64) {
        if secs >= 0.0 {
            m.duration_secs = Some(secs.round() as i64);
        }
    }

    // recording_url: the API returns `url`.
    if let Some(url) = str_opt(value, "url") {
        m.recording_url = url;
    }

    // organizer → host (email lowercased), also stash name in extra when present.
    if let Some(organizer) = obj.get("organizer") {
        if let Some(email) = str_opt(organizer, "email") {
            m.host = email.to_lowercase();
        }
        if let Some(name) = str_opt(organizer, "name") {
            m.extra.insert("organizer_name".into(), Value::from(name));
        }
    }

    // invitees[] → attendees (email lowercased) + attendee_names when aligned.
    if let Some(invitees) = obj.get("invitees").and_then(Value::as_array) {
        let emails: Vec<String> = invitees
            .iter()
            .filter_map(|i| str_opt(i, "email").map(|e| e.to_lowercase()))
            .collect();
        let names: Vec<String> =
            invitees.iter().filter_map(|i| str_opt(i, "name")).collect();
        let aligned =
            !emails.is_empty() && emails.len() == invitees.len() && names.len() == emails.len();
        if !emails.is_empty() {
            m.attendees = emails;
        }
        if aligned {
            m.attendee_names = names;
        } else if !invitees.is_empty() {
            // Names misaligned (partial) — preserve raw invitees in extra.
            m.extra.insert("invitees_raw".into(), Value::Array(invitees.clone()));
        }
    }

    // summary from the notes endpoint's markdownContent.
    if let Some(md) = notes_markdown {
        let md = md.trim();
        if !md.is_empty() {
            m.summary = md.to_string();
        }
    }

    // extraProperties.conferenceId → extra["conference_id"].
    if let Some(conf_id) = obj
        .get("extraProperties")
        .and_then(|ep| str_opt(ep, "conferenceId"))
    {
        m.extra.insert("conference_id".into(), Value::from(conf_id));
    }

    // Carry over caller-provided extra fields (topics, structuredNotes, etc.).
    for (k, v) in extra_fields {
        m.extra.entry(k).or_insert(v);
    }

    Some(m)
}

/// Extract transcript utterance segments from the transcript endpoint response.
/// Shape: `{ id, meetingId, data: [ {speaker, text, startTime, endTime} ] }`.
fn transcript_data(v: &Value) -> Vec<Value> {
    v.get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Upsert-by-guid into month partitions (the fathom/granola pattern).

fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    upsert_partition(vault, CONTRACT_DIR, rows, |m| m.ts.clone(), |m| m.guid.clone())
}

fn upsert_raw(vault: &Vault, rows: Vec<RawMeeting>) -> Result<u64> {
    upsert_partition(
        vault,
        RAW_DIR,
        rows,
        |r| r.happened_at().to_string(),
        |r| r.id(),
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
            .with_context(|| format!("tldv: ts {ts:?} has no month (dir {dir})"))?
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
        existing
            .sort_by(|a, b| ts_of(a).cmp(&ts_of(b)).then_with(|| guid_of(a).cmp(&guid_of(b))));
        vault.write_snapshot(&format!("{dir}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let key = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|k| !k.trim().is_empty())
        .context("tl;dv is not connected — add your API key in the Integrations tab")?;
    let client = TldvClient::new(API_BASE.to_string(), key);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl TldvApi) -> Result<PullOutcome> {
    let mut state = vault.read_tldv_sync();
    let watermark = state.last_happened_at.clone();
    // Recheck floor: any meeting whose happenedAt is newer than this is
    // (re)processed even when it is at-or-below the watermark. This handles the
    // common case where a just-ended meeting is listed before its transcript is
    // ready: poll 1 writes a row with no transcript_ref; subsequent polls
    // re-process the same guid (idempotent upsert) until the transcript lands.
    // On the first sync (no watermark) the floor is unbounded — write everything.
    let recheck_floor: Option<String> = watermark.as_deref().map(|w| {
        DateTime::parse_from_rfc3339(w)
            .map(|t| (t - chrono::Duration::days(RECHECK_DAYS)).to_rfc3339())
            .unwrap_or_else(|_| w.to_string())
    });

    // --- 1. Drain all pages, collecting new meeting objects. ---------------
    // tl;dv uses page-based pagination (`page`, `pages`, `results[]`).
    // We drain every page; each meeting is keyed by id (last-writer wins if
    // it appears on multiple pages, which is unlikely but safe).
    let mut meetings: BTreeMap<String, Value> = BTreeMap::new();
    let mut page = 1u32;
    loop {
        let body = api.list_meetings(PAGE_SIZE, page).map_err(|e| {
            match e {
                FetchError::Unauthorized => anyhow::anyhow!(
                    "tl;dv rejected the key (401) — reconnect from the Integrations tab"
                ),
                FetchError::Forbidden => anyhow::anyhow!(
                    "tl;dv returned 403 — API access requires the Pro or Business plan"
                ),
                FetchError::RateLimited => anyhow::anyhow!(
                    "tl;dv rate limit hit (429) — will retry on the next sync"
                ),
                other => anyhow::anyhow!("tl;dv fetch failed: {other}"),
            }
        })?;

        let (results, total_pages) = parse_list(&body);
        for item in results {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    meetings.insert(id.to_string(), item);
                }
            }
        }

        if page >= total_pages || total_pages == 0 {
            break;
        }
        page += 1;
    }

    // --- 2. For each meeting: fetch transcript + notes, build rows. --------
    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut raw_rows: Vec<RawMeeting> = Vec::new();
    let mut transcripts_written = 0u64;
    let mut newest_happened: Option<String> = watermark.clone();

    for (id, obj) in &meetings {
        let happened_at = obj.get("happenedAt").and_then(Value::as_str).unwrap_or("");
        if happened_at.is_empty() {
            // No timestamp → can't partition; skip.
            continue;
        }

        // Write-filter: (re)process a meeting when it is NEW (happened_at
        // strictly newer than the watermark) OR RECENT (happened_at within the
        // RECHECK_DAYS window). The recheck window catches transcripts that
        // were not ready on the first poll — tl;dv transcribes asynchronously,
        // so a just-ended meeting is commonly listed before its transcript is
        // available. The guid-upsert is idempotent, so re-covering a meeting
        // whose transcript is now ready is safe and backfills transcript_ref.
        // Old meetings outside the window are skipped to bound the work.
        // On the first sync (no watermark) the recheck floor is None → write
        // everything.
        let is_new = watermark.as_deref().is_none_or(|w| happened_at > w);
        let recheck =
            recheck_floor.as_deref().is_some_and(|floor| happened_at > floor);
        if !is_new && !recheck {
            // Outside both windows: already stored and past the recheck floor.
            // Still track high-water for watermark correctness.
            newest_happened = max_ts(newest_happened, happened_at.to_string());
            continue;
        }

        // Raw firehose: start with the meeting list object; we'll merge in
        // transcript + notes fields before persisting.
        let mut raw_fields = obj.as_object().cloned().unwrap_or_default();

        // Fetch transcript utterances.
        let utterances = match api.get_transcript(id) {
            Ok(v) => {
                // Merge the full transcript response into the raw object.
                raw_fields.insert("_transcript".into(), v.clone());
                transcript_data(&v)
            }
            Err(FetchError::NotFound) => Vec::new(), // transcript not ready yet
            Err(FetchError::RateLimited) => {
                return Err(anyhow::anyhow!(
                    "tl;dv rate limit hit fetching transcript for {id} — \
                     will resume from the same point on the next sync"
                ));
            }
            Err(FetchError::Unauthorized | FetchError::Forbidden) => {
                return Err(anyhow::anyhow!(
                    "tl;dv rejected credentials during transcript fetch — reconnect"
                ));
            }
            Err(e) => {
                // Transient error — keep processing; raw row still written.
                eprintln!("tldv: transcript fetch for {id} failed: {e}");
                Vec::new()
            }
        };

        // Fetch notes (summary markdown + structured notes + topics).
        let (notes_markdown, notes_extra) = match api.get_notes(id) {
            Ok(v) => {
                raw_fields.insert("_notes".into(), v.clone());
                let md = v.get("markdownContent").and_then(Value::as_str).map(str::to_owned);
                let mut extra: Map<String, Value> = Map::new();
                if let Some(topics) = v.get("topics").filter(|t| !t.is_null()) {
                    let is_empty = topics.as_array().is_some_and(|a| a.is_empty());
                    if !is_empty {
                        extra.insert("topics".into(), topics.clone());
                    }
                }
                if let Some(structured) = v.get("structuredNotes").filter(|s| !s.is_null()) {
                    let is_empty = structured.as_array().is_some_and(|a| a.is_empty());
                    if !is_empty {
                        extra.insert("structured_notes".into(), structured.clone());
                    }
                }
                (md, extra)
            }
            Err(FetchError::NotFound) => (None, Map::new()),
            Err(FetchError::RateLimited) => {
                return Err(anyhow::anyhow!(
                    "tl;dv rate limit hit fetching notes for {id} — \
                     will resume from the same point on the next sync"
                ));
            }
            Err(FetchError::Unauthorized | FetchError::Forbidden) => {
                return Err(anyhow::anyhow!(
                    "tl;dv rejected credentials during notes fetch — reconnect"
                ));
            }
            Err(e) => {
                eprintln!("tldv: notes fetch for {id} failed: {e}");
                (None, Map::new())
            }
        };

        // Persist transcript sidecar.
        let has_transcript = !utterances.is_empty();
        if has_transcript {
            write_transcript_sidecar(vault, id, &utterances)?;
            transcripts_written += 1;
        }

        // Raw firehose.
        raw_rows.push(RawMeeting { fields: raw_fields });

        // Contract row.
        let Some(mut row) =
            meeting_from_value(obj, notes_markdown.as_deref(), notes_extra)
        else {
            continue;
        };
        if has_transcript {
            row.transcript_ref = transcript_ref(id);
        }
        contract_rows.push(row);

        newest_happened = max_ts(newest_happened, happened_at.to_string());
    }

    // --- 3. Persist and advance cursor. ------------------------------------
    let raw_new = upsert_raw(vault, raw_rows)?;
    let contract_new = upsert_contract(vault, contract_rows)?;

    state.last_happened_at = newest_happened;
    vault.write_tldv_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("meetings", contract_new);
    counts.insert("transcripts", transcripts_written);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!(
            "tl;dv synced — {contract_new} new meetings, {transcripts_written} transcripts"
        ),
        counts,
    })
}

/// The later of two RFC3339 timestamps (lexical compare is correct for `…Z`).
fn max_ts(cur: Option<String>, candidate: String) -> Option<String> {
    match cur {
        Some(prev) if prev.as_str() >= candidate.as_str() => Some(prev),
        _ => Some(candidate),
    }
}

/// Write the per-meeting transcript sidecar, one utterance per JSONL line.
fn write_transcript_sidecar(vault: &Vault, meeting_id: &str, utterances: &[Value]) -> Result<()> {
    vault.write_snapshot(&transcript_ref(meeting_id), utterances)
}

/// Parse a `GET /v1alpha1/meetings` page response.
/// Returns `(results, total_pages)`.
fn parse_list(v: &Value) -> (Vec<Value>, u32) {
    match v.as_object() {
        Some(o) => {
            let results =
                o.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
            let pages = o
                .get("pages")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32;
            (results, pages)
        }
        None => {
            // Tolerate a bare array (future shape change).
            if let Some(arr) = v.as_array() {
                (arr.clone(), 1)
            } else {
                (Vec::new(), 0)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-tldv-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — shapes confirmed from doc.tldv.io + intercom.help/tldv API docs.

    /// A fully-populated meeting list item (from GET /v1alpha1/meetings results[]).
    fn meeting_obj(id: &str, happened_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": "Q3 Product Sync",
            "happenedAt": happened_at,
            "url": format!("https://tldv.io/app/meetings/{id}"),
            "duration": 2940.0,
            "organizer": {"name": "David Wills", "email": "dwills@example.com"},
            "invitees": [
                {"name": "David Wills", "email": "dwills@example.com"},
                {"name": "Sam Ortiz", "email": "sam@example.com"}
            ],
            "extraProperties": {"conferenceId": "zoom-123456789"}
        })
    }

    /// A meeting with partial invitee names (one missing) — attendee_names MUST
    /// be dropped; raw invitees preserved in extra.
    fn meeting_partial_names(id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": "Customer Call",
            "happenedAt": "2026-06-12T18:00:00Z",
            "duration": 1800.0,
            "organizer": {"email": "dwills@example.com"},
            "invitees": [
                {"name": "Jordan Lee", "email": "jordan@acme.com"},
                {"email": "pat@acme.com"}
            ]
        })
    }

    /// Transcript endpoint response shape (from doc.tldv.io).
    fn transcript_response(meeting_id: &str) -> Value {
        serde_json::json!({
            "id": "tr_001",
            "meetingId": meeting_id,
            "data": [
                {"speaker": "David Wills", "text": "Hello, let's get started.", "startTime": 0, "endTime": 3200},
                {"speaker": "Sam Ortiz", "text": "Sounds good.", "startTime": 3500, "endTime": 5000}
            ]
        })
    }

    /// Notes endpoint response shape (from doc.tldv.io).
    fn notes_response() -> Value {
        serde_json::json!({
            "markdownContent": "## Key Decisions\n- Ship the meetings contract first.",
            "topics": [
                {"id": "t1", "order": 1, "title": "Roadmap", "summary": "Q3 priorities discussed."}
            ],
            "structuredNotes": [
                {"segmentId": "s1", "timestamp": 0, "text": "Initial discussion.", "topicId": "t1"}
            ]
        })
    }

    /// A list page response with one meeting.
    fn list_page(meetings: Vec<Value>, page: u32, pages: u32) -> Value {
        serde_json::json!({
            "results": meetings,
            "page": page,
            "pages": pages,
            "total": meetings.len(),
            "pageSize": 50
        })
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        /// page number → list page response.
        pages: RefCell<Vec<(u32, Value)>>,
        /// meeting id → transcript response.
        transcripts: RefCell<std::collections::HashMap<String, Value>>,
        /// meeting id → notes response.
        notes_map: RefCell<std::collections::HashMap<String, Value>>,
        transcript_errors: RefCell<std::collections::HashSet<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                transcripts: RefCell::new(std::collections::HashMap::new()),
                notes_map: RefCell::new(std::collections::HashMap::new()),
                transcript_errors: RefCell::new(std::collections::HashSet::new()),
            }
        }

        fn add_page(&self, page: u32, v: Value) {
            self.pages.borrow_mut().push((page, v));
        }

        fn add_transcript(&self, id: &str, v: Value) {
            self.transcripts.borrow_mut().insert(id.to_string(), v);
        }

        fn add_notes(&self, id: &str, v: Value) {
            self.notes_map.borrow_mut().insert(id.to_string(), v);
        }

        fn add_transcript_error(&self, id: &str) {
            self.transcript_errors.borrow_mut().insert(id.to_string());
        }
    }

    impl TldvApi for MockApi {
        fn list_meetings(&self, _page_size: u32, page: u32) -> Result<Value, FetchError> {
            for (p, body) in self.pages.borrow().iter() {
                if *p == page {
                    return Ok(body.clone());
                }
            }
            // No page registered → empty last page.
            Ok(serde_json::json!({"results": [], "page": page, "pages": page, "total": 0, "pageSize": 50}))
        }

        fn get_transcript(&self, id: &str) -> Result<Value, FetchError> {
            if self.transcript_errors.borrow().contains(id) {
                return Err(FetchError::NotFound);
            }
            Ok(self
                .transcripts
                .borrow()
                .get(id)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"data": []})))
        }

        fn get_notes(&self, id: &str) -> Result<Value, FetchError> {
            Ok(self
                .notes_map
                .borrow()
                .get(id)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"markdownContent": null, "topics": [], "structuredNotes": []})))
        }
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_meeting_guid_title_duration_attendees_host() {
        let obj = meeting_obj("m001", "2026-06-10T16:00:00Z");
        let m = meeting_from_value(&obj, None, Map::new()).unwrap();
        assert_eq!(m.source, "tldv");
        assert_eq!(m.guid, "m001");
        assert_eq!(m.title, "Q3 Product Sync");
        assert_eq!(m.duration_secs, Some(2940));
        assert_eq!(
            DateTime::parse_from_rfc3339(&m.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T16:00:00Z").unwrap().timestamp(),
        );
        assert_eq!(m.attendees, vec!["dwills@example.com", "sam@example.com"]);
        assert_eq!(m.attendee_names, vec!["David Wills", "Sam Ortiz"]);
        assert_eq!(m.host, "dwills@example.com");
        assert!(m.recording_url.contains("m001"));
        assert_eq!(m.extra.get("conference_id").and_then(Value::as_str), Some("zoom-123456789"));
        assert_eq!(m.extra.get("organizer_name").and_then(Value::as_str), Some("David Wills"));
    }

    #[test]
    fn partial_invitee_names_fall_to_extra_no_attendee_names() {
        let obj = meeting_partial_names("m002");
        let m = meeting_from_value(&obj, None, Map::new()).unwrap();
        assert_eq!(m.attendees, vec!["jordan@acme.com", "pat@acme.com"]);
        assert!(m.attendee_names.is_empty(), "misaligned names must not be written");
        assert!(m.extra.contains_key("invitees_raw"), "raw invitees preserved in extra");
    }

    #[test]
    fn notes_markdown_populates_summary_and_topics_go_to_extra() {
        let obj = meeting_obj("m003", "2026-06-11T10:00:00Z");
        let notes = notes_response();
        let md = notes.get("markdownContent").and_then(Value::as_str);
        let mut extra: Map<String, Value> = Map::new();
        extra.insert("topics".into(), notes["topics"].clone());

        let m = meeting_from_value(&obj, md, extra).unwrap();
        assert!(m.summary.contains("Ship the meetings contract first"));
        assert!(m.extra.contains_key("topics"));
    }

    #[test]
    fn transcript_data_extracts_utterances() {
        let resp = transcript_response("m001");
        let utterances = transcript_data(&resp);
        assert_eq!(utterances.len(), 2);
        assert_eq!(utterances[0].get("speaker").and_then(Value::as_str), Some("David Wills"));
        assert_eq!(utterances[0].get("text").and_then(Value::as_str), Some("Hello, let's get started."));
        // startTime/endTime are numbers.
        assert_eq!(utterances[0].get("startTime").and_then(Value::as_u64), Some(0));
        assert_eq!(utterances[1].get("endTime").and_then(Value::as_u64), Some(5000));
    }

    #[test]
    fn empty_transcript_data_returns_empty_vec() {
        let resp = serde_json::json!({"data": []});
        assert!(transcript_data(&resp).is_empty());
        let resp2 = serde_json::json!({"data": null});
        assert!(transcript_data(&resp2).is_empty());
    }

    #[test]
    fn transcript_ref_path_is_per_meeting_sidecar() {
        assert_eq!(transcript_ref("m001"), "meetings/tldv/raw/transcripts/m001.jsonl");
    }

    #[test]
    fn parse_list_reads_results_and_pages() {
        let page = list_page(vec![meeting_obj("m1", "2026-06-10T16:00:00Z")], 1, 3);
        let (results, pages) = parse_list(&page);
        assert_eq!(results.len(), 1);
        assert_eq!(pages, 3);

        // Empty terminal page.
        let empty = serde_json::json!({"results": [], "page": 3, "pages": 3, "total": 0, "pageSize": 50});
        let (results2, pages2) = parse_list(&empty);
        assert!(results2.is_empty());
        assert_eq!(pages2, 3);

        // Bare array tolerance.
        let bare = serde_json::json!([meeting_obj("m2", "2026-06-10T17:00:00Z")]);
        let (results3, pages3) = parse_list(&bare);
        assert_eq!(results3.len(), 1);
        assert_eq!(pages3, 1);
    }

    // -----------------------------------------------------------------------
    // Integration tests (full pull_with).

    #[test]
    fn full_pull_writes_contract_raw_transcript_and_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::new();
        api.add_page(1, list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 1));
        api.add_transcript("m001", transcript_response("m001"));
        api.add_notes("m001", notes_response());

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1));
        assert_eq!(out.counts.get("transcripts"), Some(&1));
        assert_eq!(out.counts.get("raw"), Some(&1));

        // Contract row in the correct month partition.
        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.guid, "m001");
        assert_eq!(m.transcript_ref, "meetings/tldv/raw/transcripts/m001.jsonl");
        assert!(m.summary.contains("Ship the meetings contract first"));
        assert_eq!(m.attendees, vec!["dwills@example.com", "sam@example.com"]);
        assert_eq!(m.duration_secs, Some(2940));

        // Transcript sidecar exists with correct utterances.
        let sidecar = v.root().join("meetings/tldv/raw/transcripts/m001.jsonl");
        assert!(sidecar.exists());
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(body.lines().count(), 2, "two utterances, one per line");
        assert!(body.contains("\"speaker\":\"David Wills\""));
        assert!(body.contains("\"startTime\":0"));

        // Raw firehose.
        let raw_path = v.root().join("meetings/tldv/raw/2026-06.jsonl");
        assert!(raw_path.exists());
        let raw_body = std::fs::read_to_string(&raw_path).unwrap();
        assert!(raw_body.contains("\"id\":\"m001\""));

        // Cursor advanced.
        let state = v.read_tldv_sync();
        assert_eq!(state.last_happened_at.as_deref(), Some("2026-06-10T16:00:00Z"));
    }

    #[test]
    fn second_poll_skips_already_seen_meeting() {
        let v = temp_vault("secondpoll");
        let api = MockApi::new();
        api.add_page(1, list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 1));
        api.add_transcript("m001", transcript_response("m001"));
        api.add_notes("m001", notes_response());

        // First pull.
        pull_with(&v, &api).unwrap();

        // Second pull — same meeting is below watermark.
        let api2 = MockApi::new();
        api2.add_page(1, list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 1));
        let out2 = pull_with(&v, &api2).unwrap();

        // No new meetings upserted.
        assert_eq!(out2.counts.get("meetings"), Some(&0));
        assert_eq!(out2.counts.get("raw"), Some(&0));

        // Still only one row.
        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn new_meeting_above_watermark_is_written() {
        let v = temp_vault("watermark");
        let api = MockApi::new();
        api.add_page(1, list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 1));
        api.add_transcript("m001", transcript_response("m001"));
        api.add_notes("m001", notes_response());
        pull_with(&v, &api).unwrap();

        // New meeting after the watermark.
        let api2 = MockApi::new();
        api2.add_page(
            1,
            list_page(
                vec![
                    meeting_obj("m001", "2026-06-10T16:00:00Z"),
                    meeting_obj("m002", "2026-06-11T09:00:00Z"),
                ],
                1,
                1,
            ),
        );
        api2.add_transcript("m002", transcript_response("m002"));
        api2.add_notes("m002", notes_response());
        let out2 = pull_with(&v, &api2).unwrap();

        assert_eq!(out2.counts.get("meetings"), Some(&1), "only m002 is new");
        assert_eq!(out2.counts.get("transcripts"), Some(&1));

        let state = v.read_tldv_sync();
        assert_eq!(state.last_happened_at.as_deref(), Some("2026-06-11T09:00:00Z"));
    }

    #[test]
    fn multi_page_drain_collects_all_meetings() {
        let v = temp_vault("multipage");
        let api = MockApi::new();
        // Two pages.
        api.add_page(
            1,
            list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 2),
        );
        api.add_page(
            2,
            list_page(vec![meeting_obj("m002", "2026-06-11T09:00:00Z")], 2, 2),
        );
        api.add_transcript("m001", transcript_response("m001"));
        api.add_transcript("m002", transcript_response("m002"));
        api.add_notes("m001", notes_response());
        api.add_notes("m002", notes_response());

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&2), "both meetings written");
        assert_eq!(out.counts.get("transcripts"), Some(&2));

        // Watermark = the later meeting.
        let state = v.read_tldv_sync();
        assert_eq!(state.last_happened_at.as_deref(), Some("2026-06-11T09:00:00Z"));
    }

    #[test]
    fn missing_transcript_no_transcript_ref() {
        let v = temp_vault("notranscript");
        let api = MockApi::new();
        api.add_page(1, list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 1));
        // No transcript registered → mock returns empty data[].
        api.add_notes("m001", notes_response());

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("transcripts"), Some(&0));

        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows[0].transcript_ref, "", "no sidecar → transcript_ref empty");
        assert!(!v.root().join("meetings/tldv/raw/transcripts/m001.jsonl").exists());
    }

    /// Regression for the blocking defect: tl;dv transcribes asynchronously,
    /// so a just-ended meeting is commonly listed before its transcript is
    /// ready.  Poll 1 writes a row with no transcript_ref; poll 2 (same
    /// happenedAt — unchanged) must still re-check the meeting because it is
    /// within the RECHECK_DAYS window, fetch the now-ready transcript, upsert
    /// the row in place with transcript_ref set, and write the sidecar.
    #[test]
    fn late_transcript_backfilled_on_second_poll() {
        let v = temp_vault("latetranscript");

        // Poll 1: meeting listed, transcript not ready (404 → NotFound).
        let api1 = MockApi::new();
        api1.add_page(1, list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 1));
        api1.add_transcript_error("m001"); // transcript 404 on first poll
        api1.add_notes("m001", notes_response());

        let out1 = pull_with(&v, &api1).unwrap();
        assert_eq!(out1.counts.get("transcripts"), Some(&0), "poll 1: transcript not ready");
        assert_eq!(out1.counts.get("meetings"), Some(&1), "poll 1: contract row written");

        // Contract row present but transcript_ref empty.
        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows1: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows1.len(), 1);
        assert_eq!(rows1[0].transcript_ref, "", "poll 1: transcript_ref must be empty");
        assert!(!v.root().join("meetings/tldv/raw/transcripts/m001.jsonl").exists());

        // Poll 2: same meeting at same happenedAt, transcript now ready.
        // The watermark was advanced to "2026-06-10T16:00:00Z" by poll 1, so
        // is_new = false; but the RECHECK_DAYS window must still pick it up.
        let api2 = MockApi::new();
        api2.add_page(1, list_page(vec![meeting_obj("m001", "2026-06-10T16:00:00Z")], 1, 1));
        api2.add_transcript("m001", transcript_response("m001")); // transcript ready now
        api2.add_notes("m001", notes_response());

        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("transcripts"), Some(&1), "poll 2: transcript must be backfilled");

        // Upserted row now has transcript_ref set, still one row (no duplicate).
        let rows2: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows2.len(), 1, "upsert must not duplicate the row");
        assert_eq!(
            rows2[0].transcript_ref,
            "meetings/tldv/raw/transcripts/m001.jsonl",
            "poll 2: transcript_ref backfilled"
        );
        assert!(
            v.root().join("meetings/tldv/raw/transcripts/m001.jsonl").exists(),
            "sidecar written on poll 2"
        );
    }

    /// Meetings outside the RECHECK_DAYS window are not re-processed: they are
    /// already stored and their transcripts will never arrive (or we gave up).
    /// This test uses a past date far outside any 30-day window to prove the
    /// skip still triggers correctly for genuinely old meetings.
    #[test]
    fn ancient_meeting_outside_recheck_window_not_reprocessed() {
        let v = temp_vault("ancientmeeting");

        // Poll 1: two meetings — one recent, one ancient.
        let recent_ts = "2026-06-10T16:00:00Z";
        let ancient_ts = "2025-01-01T10:00:00Z"; // > 30 days before recent
        let api1 = MockApi::new();
        api1.add_page(
            1,
            list_page(
                vec![
                    meeting_obj("recent", recent_ts),
                    meeting_obj("ancient", ancient_ts),
                ],
                1,
                1,
            ),
        );
        api1.add_transcript("recent", transcript_response("recent"));
        api1.add_transcript("ancient", transcript_response("ancient"));
        api1.add_notes("recent", notes_response());
        api1.add_notes("ancient", notes_response());
        pull_with(&v, &api1).unwrap();

        // Poll 2: same meetings again; ancient is outside the RECHECK_DAYS window.
        let api2 = MockApi::new();
        api2.add_page(
            1,
            list_page(
                vec![
                    meeting_obj("recent", recent_ts),
                    meeting_obj("ancient", ancient_ts),
                ],
                1,
                1,
            ),
        );
        api2.add_transcript("recent", transcript_response("recent"));
        api2.add_transcript("ancient", transcript_response("ancient"));
        api2.add_notes("recent", notes_response());
        api2.add_notes("ancient", notes_response());
        let out2 = pull_with(&v, &api2).unwrap();

        // recent is at watermark so still inside recheck window; ancient is
        // far outside (> RECHECK_DAYS ago from watermark). Neither is "new"
        // (both at-or-below watermark). recent may be upserted (idempotent);
        // ancient must NOT be re-processed.
        // Both were already written on poll 1 → new upserted count should be 0.
        assert_eq!(out2.counts.get("meetings"), Some(&0), "no new rows on second poll");
    }

    #[test]
    fn meeting_without_happened_at_is_skipped() {
        let v = temp_vault("nohappenedat");
        let api = MockApi::new();
        let bad = serde_json::json!({"id": "mXXX", "name": "No date"});
        api.add_page(1, list_page(vec![bad], 1, 1));

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&0));
        assert_eq!(out.counts.get("raw"), Some(&0));
    }
}
