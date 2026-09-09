//! Read.ai — AI meeting notetaker with an open-beta REST API.
//!
//! Pulls completed meetings — transcripts, summaries, action items, and
//! speaker analytics — via `GET /v1/meetings` (base URL `https://api.read.ai`),
//! using a per-user API key (Bearer token). The API is open beta; this module
//! pins `v1` and treats unknown fields as `extra` so beta churn degrades
//! gracefully.
//!
//! Three vault destinations, written in one pass:
//!
//! - **meetings contract** under `meetings/read-ai/YYYY-MM.jsonl`: one
//!   [`crate::meetings::Meeting`] per meeting, upserted by `guid` (= the
//!   Read.ai `id` string). Re-polls are idempotent; a transcript that arrives
//!   later updates the same row in place.
//! - **raw meetings** under `meetings/read-ai/raw/YYYY-MM.jsonl`: the verbatim
//!   API meeting object at full fidelity (partitioned by start month, upserted
//!   by `id`).
//! - **transcript sidecars** under `meetings/read-ai/raw/transcripts/<id>.md`:
//!   the meeting's `transcript` text (a string), written once and updated
//!   when a transcript arrives on a later poll. `transcript_ref` on the
//!   contract row points here.
//!
//! ## API shape (confirmed from the `@hyperdrive.bot/read-ai` npm package,
//! which wraps the official open-beta REST API; support article 49381161088659)
//!
//! - `GET /v1/meetings?limit=N&cursor=<id>&expand[]=summary&expand[]=transcript&expand[]=action_items`
//! - Response: `{ "object": "list", "url": "...", "has_more": bool, "data": [...] }`
//! - Cursor: the `id` of the **last** item in `data` — NOT a separate field.
//!   Drain while `has_more && data.len() > 0`.
//! - Meeting object: `id` (string, stable), `title`, `start_time_ms` / `end_time_ms`
//!   (Unix-epoch milliseconds), `participants: [{name, email, invited, attended}]`,
//!   `owner: {name, email}`, `report_url`, `platform`, `platform_id`, `folders: [string]`,
//!   `summary` (expanded; string), `transcript` (expanded; string),
//!   `action_items` (expanded; string[]).
//!
//! ## Cursor
//!
//! `.trove/read-ai-sync.json` holds `last_meeting_ts` (RFC3339 UTC of the
//! newest meeting we've stored **with a transcript**), used as a write-filter
//! watermark — not a traversal cut-off. We always drain every page, then write
//! a meeting when it is new (start after watermark) or within
//! [`RECHECK_DAYS`] of it (transcript may have arrived). The cursor carries no
//! secrets.
//!
//! ## Privacy
//!
//! Meeting transcripts are conversation content. `default_on: false` — the hub
//! renders the opt-in gate.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/read-ai.md.

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

/// Collector id and source folder name.
const SOURCE: &str = "read-ai";
/// Contract rows (one Meeting per meeting, upserted by guid).
const CONTRACT_DIR: &str = "meetings/read-ai";
/// Raw firehose (verbatim API objects).
const RAW_DIR: &str = "meetings/read-ai/raw";
/// Per-meeting transcript sidecars.
const TRANSCRIPT_DIR: &str = "meetings/read-ai/raw/transcripts";
/// Non-secret rebuildable cursor (NOT under `.trove/sync/`).
const SYNC_FILE: &str = ".trove/read-ai-sync.json";
/// Service key under `.trove/sync/` — the Bearer token rides `access_token`.
const SERVICE: &str = "read-ai";
/// API base URL (open-beta v1).
const API_BASE: &str = "https://api.read.ai";
/// Connection timeout per call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Periodic cadence: every 30 minutes (conservative; no rate-limit docs).
pub const READ_AI_SYNC_SECS: u64 = 1800;
/// How many days behind the watermark a still-incomplete meeting is re-checked.
const RECHECK_DAYS: i64 = 30;
/// Page size (max 100 per npm client reference).
const PAGE_SIZE: usize = 100;
/// Expand fields requested on every list call (percent-encoded brackets).
const EXPAND: &str = "expand%5B%5D=summary&expand%5B%5D=transcript&expand%5B%5D=action_items";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_read_ai_sync().last_meeting_ts.filter(|s| !s.is_empty())
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "read-ai synced — {} meetings, {} transcripts",
                    c("meetings"),
                    c("transcripts"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "read-ai sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "read-ai",
        name: "Read.ai",
        kind: IntegrationKind::CloudSync,
        // 🔒 Opt-in: meeting transcripts are conversation content.
        default_on: false,
        description: "Pulls your Read.ai meeting reports — transcripts, summaries, and action \
                      items — via the open-beta REST API (api.read.ai/v1), every 30 minutes. \
                      Uses a per-user API key; no paid plan required.",
        domain: "meetings",
        vault_path: "meetings/read-ai/",
        toggleable: true,
        setup: &[
            "Connect with your Read.ai API key on this card.",
            "Each sync pulls new meetings; transcripts attach once Read.ai finishes processing.",
        ],
        caveats: "Meeting transcripts are conversation content, so this source is off by \
                  default — turn it on deliberately. The API is in open beta; breaking changes \
                  are possible.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(READ_AI_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("read-ai"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = Bearer token API key).

fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key — paste your Read.ai API key");
    }
    let client = ReadAiClient::new(API_BASE.to_string(), key.to_string());
    // Verify with a cheap 1-item list call.
    match client.get_json(&format!("/v1/meetings?limit=1&{EXPAND}")) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Read.ai rejected the key (401) — check it's your API key from \
             Settings → API and hasn't been revoked"
        ),
        Err(e) => bail!("Read.ai /v1/meetings check failed: {e}"),
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
            label: "Read.ai".to_string(),
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
    id: "read-ai",
    display_name: "Read.ai",
    methods: &[ConnectMethod::TokenPaste {
        label: "Read.ai API key",
        help: "Read.ai → Settings → API → generate an API key.",
        placeholder: "readai_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["read-ai"],
    setup: &[
        "In Read.ai, open Settings → API.",
        "Generate an API key.",
        "Paste it here — it's stored locally (0600) and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable trait so tests run fully offline.

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

trait ReadAiApi {
    fn get_json(&self, path: &str) -> Result<Value, FetchError>;
}

struct ReadAiClient {
    base: String,
    token: String,
}

impl ReadAiClient {
    fn new(base: String, token: String) -> Self {
        ReadAiClient { base, token }
    }
}

impl ReadAiApi for ReadAiClient {
    fn get_json(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .call();
        match resp {
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

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct SyncState {
    /// RFC3339 UTC of the newest meeting we've stored **with a transcript**.
    /// Write-filter watermark — never a traversal cut-off. Not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_meeting_ts: Option<String>,
}

impl Vault {
    fn read_read_ai_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_read_ai_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (verbatim API meeting object).

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawMeeting {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawMeeting {
    fn id(&self) -> String {
        self.fields.get("id").and_then(Value::as_str).unwrap_or("").to_string()
    }

    /// Start timestamp as RFC3339 UTC (converted from `start_time_ms`).
    fn start_rfc3339(&self) -> String {
        self.fields
            .get("start_time_ms")
            .and_then(Value::as_i64)
            .map(ms_to_rfc3339)
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Pure mapping helpers.

/// Unix-epoch milliseconds → RFC3339 UTC string.
fn ms_to_rfc3339(ms: i64) -> String {
    use chrono::TimeZone;
    let secs = ms / 1000;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    chrono::Utc
        .timestamp_opt(secs, nanos)
        .single()
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

/// RFC3339 UTC string → RFC3339 **local**.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Duration in seconds from `start_time_ms` / `end_time_ms`. `None` when
/// either field is absent or the result is negative.
fn duration_from_ms(start_ms: i64, end_ms_opt: Option<i64>) -> Option<i64> {
    let end_ms = end_ms_opt?;
    let secs = (end_ms - start_ms) / 1000;
    (secs >= 0).then_some(secs)
}

/// Map a raw API meeting object → a normalized [`Meeting`] (without
/// `transcript_ref`). Returns `None` when the object has no usable `id` or no
/// usable `start_time_ms` (can't partition / dedup).
fn meeting_from_value(value: &Value) -> Option<Meeting> {
    let obj = value.as_object()?;
    let guid = obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;

    let start_ms = obj.get("start_time_ms").and_then(Value::as_i64)?;
    let end_ms = obj.get("end_time_ms").and_then(Value::as_i64);

    let start_rfc = ms_to_rfc3339(start_ms);
    if start_rfc.is_empty() {
        return None;
    }
    let end_rfc = end_ms.map(ms_to_rfc3339).filter(|s| !s.is_empty());

    let mut m = Meeting::new(SOURCE, guid, to_local(&start_rfc));
    m.started = to_local(&start_rfc);
    if let Some(ref end) = end_rfc {
        m.ended = to_local(end);
    }
    m.duration_secs = duration_from_ms(start_ms, end_ms);

    if let Some(title) = obj.get("title").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        m.title = title.to_string();
    }

    if let Some(platform) = obj.get("platform").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        m.platform = platform.to_string();
    }

    // attendees from participants[].email (lowercased); names ONLY when fully
    // aligned (every participant has both email and name).
    if let Some(participants) = obj.get("participants").and_then(Value::as_array) {
        let emails: Vec<String> = participants
            .iter()
            .filter_map(|p| {
                p.get("email")
                    .and_then(Value::as_str)
                    .map(|e| e.trim().to_lowercase())
                    .filter(|e| !e.is_empty())
            })
            .collect();
        let names: Vec<String> = participants
            .iter()
            .filter_map(|p| {
                p.get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
            })
            .collect();
        let aligned = !emails.is_empty()
            && emails.len() == participants.len()
            && names.len() == emails.len();
        if !emails.is_empty() {
            m.attendees = emails;
        }
        if aligned {
            m.attendee_names = names;
        } else if !participants.is_empty() {
            m.extra.insert("participants".into(), Value::Array(participants.clone()));
        }
    }

    // host = owner.email (lowercased)
    if let Some(owner) = obj.get("owner") {
        if let Some(email) = owner
            .get("email")
            .and_then(Value::as_str)
            .map(|e| e.trim().to_lowercase())
            .filter(|e| !e.is_empty())
        {
            m.host = email;
        }
        if let Some(name) = owner.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
            m.extra.insert("owner_name".into(), Value::String(name.to_string()));
        }
    }

    // summary (expanded — plain string)
    if let Some(summary) = obj.get("summary").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        m.summary = summary.to_string();
    }

    // report_url → recording_url (durable link to the Read.ai report page)
    if let Some(url) = obj.get("report_url").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m.recording_url = url.to_string();
    }

    // folders[] → folder (first entry; full list in extra when >1)
    if let Some(folders) = obj.get("folders").and_then(Value::as_array) {
        if let Some(first) = folders.first().and_then(Value::as_str).filter(|s| !s.is_empty()) {
            m.folder = first.to_string();
        }
        if folders.len() > 1 {
            m.extra.insert("folders".into(), Value::Array(folders.clone()));
        }
    }

    // action_items[] → extra (not a contract field)
    if let Some(items) = obj.get("action_items").filter(|v| !v.is_null()) {
        let is_empty = items.as_array().is_some_and(|a| a.is_empty());
        if !is_empty {
            m.extra.insert("action_items".into(), items.clone());
        }
    }

    // platform_id → extra
    if let Some(pid) = obj.get("platform_id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m.extra.insert("platform_id".into(), Value::String(pid.to_string()));
    }

    Some(m)
}

// ---------------------------------------------------------------------------
// Transcript sidecar.

/// Vault-relative path for the transcript sidecar file for a given meeting id.
fn transcript_ref(id: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{id}.md")
}

/// Extract the `transcript` string from a meeting object (expanded field).
/// Empty/null/absent → `None` (transcript not yet processed).
fn transcript_text(value: &Value) -> Option<&str> {
    value
        .get("transcript")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Write the transcript sidecar as a plain-text Markdown file (atomic rename).
fn write_transcript_sidecar(vault: &Vault, id: &str, text: &str) -> Result<()> {
    let path = vault.resolve(&transcript_ref(id))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Upsert-by-id into month partitions (the fathom pattern).

fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    upsert_partition(vault, CONTRACT_DIR, rows, |m| m.ts.clone(), |m| m.guid.clone())
}

fn upsert_raw(vault: &Vault, rows: Vec<RawMeeting>) -> Result<u64> {
    upsert_partition(vault, RAW_DIR, rows, |r| r.start_rfc3339(), |r| r.id())
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
            .with_context(|| format!("read-ai: ts {ts:?} has no month (dir {dir})"))?
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
// Pull.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|k| !k.trim().is_empty())
        .context("Read.ai is not connected — add your API key in the Integrations tab")?;
    let client = ReadAiClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl ReadAiApi) -> Result<PullOutcome> {
    let mut state = vault.read_read_ai_sync();
    let watermark = state.last_meeting_ts.clone();
    let recheck_floor: Option<String> = watermark.as_deref().map(|w| {
        DateTime::parse_from_rfc3339(w)
            .map(|t| (t - chrono::Duration::days(RECHECK_DAYS)).to_rfc3339())
            .unwrap_or_else(|_| w.to_string())
    });

    // --- 1. drain every page (cursor = last item's id; drain while has_more) -
    // Always drain the full list — sort order is not documented, so stopping
    // early could lose meetings on later pages (the data-loss guard).
    let mut meetings: BTreeMap<String, Value> = BTreeMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!(
                "/v1/meetings?limit={PAGE_SIZE}&{EXPAND}&cursor={}",
                urlencode(c)
            ),
            None => format!("/v1/meetings?limit={PAGE_SIZE}&{EXPAND}"),
        };
        let body = api.get_json(&path).map_err(fetch_err)?;
        let (items, has_more) = parse_list(&body);
        let last_id = items
            .last()
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        for item in items {
            if let Some(id) =
                item.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())
            {
                meetings.insert(id.to_string(), item);
            }
        }
        if has_more {
            if let Some(id) = last_id {
                cursor = Some(id);
                continue;
            }
        }
        break;
    }

    // --- 2. per meeting: build contract row + raw + transcript sidecar -------
    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut raw_rows: Vec<RawMeeting> = Vec::new();
    let mut transcripts_written = 0u64;
    let mut newest_with_transcript: Option<String> = watermark.clone();

    for (id, obj) in &meetings {
        let start_ms = obj.get("start_time_ms").and_then(Value::as_i64).unwrap_or(0);
        let start_rfc = if start_ms > 0 { ms_to_rfc3339(start_ms) } else { String::new() };
        let transcript = transcript_text(obj);
        let has_transcript = transcript.is_some();

        // Write-filter: write when the meeting is new (start after watermark)
        // or recent (within RECHECK_DAYS window). Full drain always; the filter
        // bounds writes, not traversal.
        let is_new = watermark.as_deref().is_none_or(|w| start_rfc.as_str() > w);
        let recheck =
            recheck_floor.as_deref().is_some_and(|floor| start_rfc.as_str() > floor);
        if !is_new && !recheck {
            if has_transcript && !start_rfc.is_empty() {
                newest_with_transcript = max_ts(newest_with_transcript, start_rfc);
            }
            continue;
        }

        // Raw firehose: verbatim API object, full fidelity.
        raw_rows.push(RawMeeting {
            fields: obj.as_object().cloned().unwrap_or_default(),
        });

        let Some(mut row) = meeting_from_value(obj) else {
            continue;
        };

        if let Some(text) = transcript {
            write_transcript_sidecar(vault, id, text)?;
            transcripts_written += 1;
            row.transcript_ref = transcript_ref(id);
            if !start_rfc.is_empty() {
                newest_with_transcript = max_ts(newest_with_transcript, start_rfc);
            }
        }
        contract_rows.push(row);
    }

    // --- 3. persist: raw firehose + contract upsert + cursor ---------------
    let raw_new = upsert_raw(vault, raw_rows)?;
    let contract_new = upsert_contract(vault, contract_rows)?;

    state.last_meeting_ts = newest_with_transcript;
    vault.write_read_ai_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("meetings", contract_new);
    counts.insert("transcripts", transcripts_written);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!(
            "Read.ai synced — {contract_new} new meetings, {transcripts_written} transcripts"
        ),
        counts,
    })
}

/// The later of two RFC3339 start times (lexical compare is correct for the
/// UTC `…Z` form).
fn max_ts(cur: Option<String>, candidate: String) -> Option<String> {
    match cur {
        Some(prev) if prev.as_str() >= candidate.as_str() => Some(prev),
        _ => Some(candidate),
    }
}

/// Parse a Read.ai list response:
/// `{ "object": "list", "url": "...", "has_more": bool, "data": [...] }`.
/// Returns `(items, has_more)`. Tolerates a bare array.
fn parse_list(v: &Value) -> (Vec<Value>, bool) {
    match v {
        Value::Object(o) => {
            let items = o.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            let has_more = o.get("has_more").and_then(Value::as_bool).unwrap_or(false);
            (items, has_more)
        }
        Value::Array(a) => (a.clone(), false),
        _ => (Vec::new(), false),
    }
}

fn fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => {
            anyhow::anyhow!(
                "Read.ai rejected the key (401) — reconnect from the Integrations tab"
            )
        }
        FetchError::RateLimited => {
            anyhow::anyhow!("Read.ai rate limit hit (429) — will retry on the next sync")
        }
        other => anyhow::anyhow!("Read.ai fetch failed: {other}"),
    }
}

/// Minimal percent-encoding for query/path values.
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

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashSet;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-read-ai-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ------------------------------------------------------------------
    // Test fixtures (confirmed field names from @hyperdrive.bot/read-ai npm
    // package TypeScript types + read-ai-client.js source).
    // ------------------------------------------------------------------

    /// A full meeting object with transcript, summary, and action items.
    fn meeting_full(id: &str) -> Value {
        json!({
            "id": id,
            "title": "Q3 Roadmap Sync",
            "start_time_ms": 1749571200000i64,   // 2025-06-10T16:00:00Z
            "end_time_ms":   1749574140000i64,   // 2025-06-10T16:49:00Z (49 min = 2940 s)
            "participants": [
                { "name": "Alice Smith", "email": "alice@example.com", "invited": true, "attended": true },
                { "name": "Bob Jones",   "email": "bob@example.com",   "invited": true, "attended": true }
            ],
            "owner": { "name": "Alice Smith", "email": "alice@example.com" },
            "report_url": "https://app.read.ai/meetings/abc123",
            "platform": "zoom",
            "platform_id": "zoom-meeting-99",
            "folders": ["Work"],
            "summary": "## Decisions\n- Ship the meetings contract first",
            "transcript": "Alice: Let's align on the roadmap.\nBob: Agreed, shipping the contract is priority.",
            "action_items": ["Send SOC2 report", "Schedule follow-up"]
        })
    }

    /// A meeting whose transcript is not yet processed.
    fn meeting_pending(id: &str) -> Value {
        json!({
            "id": id,
            "title": "Standup",
            "start_time_ms": 1749657600000i64,   // 2025-06-11T16:00:00Z
            "end_time_ms":   1749658500000i64,
            "participants": [
                { "name": "Alice Smith", "email": "alice@example.com", "invited": true, "attended": true }
            ],
            "owner": { "name": "Alice Smith", "email": "alice@example.com" },
            "report_url": "https://app.read.ai/meetings/pending1",
            "platform": "teams"
        })
    }

    /// The same pending meeting, now with its transcript.
    fn meeting_pending_with_transcript(id: &str) -> Value {
        let mut m = meeting_pending(id);
        m.as_object_mut().unwrap().insert(
            "transcript".into(),
            json!("Alice: Daily update — no blockers."),
        );
        m
    }

    // ------------------------------------------------------------------
    // Mock API
    // ------------------------------------------------------------------

    struct MockApi {
        /// (cursor_in → response). `None` cursor = first page.
        pages: RefCell<Vec<(Option<String>, Value)>>,
        requested: RefCell<HashSet<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                requested: RefCell::new(HashSet::new()),
            }
        }

        fn add_page(&self, cursor: Option<&str>, items: Vec<Value>, has_more: bool) {
            let cursor_key = cursor.map(str::to_string);
            let response = json!({
                "object": "list",
                "url": "/v1/meetings",
                "has_more": has_more,
                "data": items
            });
            self.pages.borrow_mut().push((cursor_key, response));
        }

        fn requested(&self, fragment: &str) -> bool {
            self.requested.borrow().iter().any(|r| r.contains(fragment))
        }
    }

    impl ReadAiApi for MockApi {
        fn get_json(&self, path: &str) -> Result<Value, FetchError> {
            self.requested.borrow_mut().insert(path.to_string());
            // Find the matching page by cursor= parameter.
            let cursor_val: Option<String> = path
                .split('&')
                .find(|s| s.starts_with("cursor="))
                .map(|s| s["cursor=".len()..].to_string());
            for (key, resp) in self.pages.borrow().iter() {
                if *key == cursor_val {
                    return Ok(resp.clone());
                }
            }
            // No matching page → empty terminal.
            Ok(json!({ "object": "list", "url": "/v1/meetings", "has_more": false, "data": [] }))
        }
    }

    // ------------------------------------------------------------------
    // Unit tests
    // ------------------------------------------------------------------

    #[test]
    fn ms_to_rfc3339_converts_epoch_millis() {
        let rfc = ms_to_rfc3339(1749571200000);
        // 2025-06-10T16:00:00Z
        assert!(rfc.contains("2025-06-10"), "got: {rfc}");
        assert!(rfc.contains("16:00:00"), "got: {rfc}");
    }

    #[test]
    fn duration_from_ms_computes_and_rejects_inverted() {
        assert_eq!(duration_from_ms(1749571200000, Some(1749574140000)), Some(2940));
        assert_eq!(duration_from_ms(1749571200000, None), None);
        assert_eq!(
            duration_from_ms(1749574140000, Some(1749571200000)),
            None,
            "inverted → none"
        );
    }

    #[test]
    fn maps_all_core_fields() {
        let m = meeting_from_value(&meeting_full("m1")).unwrap();
        assert_eq!(m.source, "read-ai");
        assert_eq!(m.guid, "m1");
        assert_eq!(m.title, "Q3 Roadmap Sync");
        assert_eq!(m.platform, "zoom");
        // Duration: 49 min = 2940 s
        assert_eq!(m.duration_secs, Some(2940));
        // Attendees: both emails, lowercased; names aligned.
        assert_eq!(m.attendees, vec!["alice@example.com", "bob@example.com"]);
        assert_eq!(m.attendee_names, vec!["Alice Smith", "Bob Jones"]);
        // host = owner.email
        assert_eq!(m.host, "alice@example.com");
        // summary
        assert!(m.summary.contains("Ship the meetings contract first"));
        // recording_url = report_url
        assert_eq!(m.recording_url, "https://app.read.ai/meetings/abc123");
        // folder = first folder entry
        assert_eq!(m.folder, "Work");
        // action_items → extra
        assert!(m.extra.contains_key("action_items"));
        // platform_id → extra
        assert!(m.extra.contains_key("platform_id"));
        // transcript_ref NOT set by mapping (set by pull loop)
        assert_eq!(m.transcript_ref, "");
    }

    #[test]
    fn missing_id_or_start_returns_none() {
        let v = json!({ "title": "X", "start_time_ms": 1749571200000i64 });
        assert!(meeting_from_value(&v).is_none(), "no id → None");
        let v = json!({ "id": "x", "title": "X" });
        assert!(meeting_from_value(&v).is_none(), "no start_time_ms → None");
    }

    #[test]
    fn partial_names_do_not_produce_attendee_names() {
        let v = json!({
            "id": "p1",
            "title": "Call",
            "start_time_ms": 1749571200000i64,
            "participants": [
                { "email": "a@example.com", "name": "Alice", "invited": true, "attended": true },
                { "email": "b@example.com", "invited": true, "attended": true }
            ],
            "owner": { "name": "Alice", "email": "a@example.com" }
        });
        let m = meeting_from_value(&v).unwrap();
        assert!(m.attendee_names.is_empty(), "misaligned names → no attendee_names");
        assert!(m.extra.contains_key("participants"), "raw participants in extra");
    }

    #[test]
    fn transcript_text_extracts_or_none() {
        assert!(transcript_text(&meeting_full("x")).is_some());
        assert!(transcript_text(&meeting_pending("x")).is_none());
        assert_eq!(transcript_text(&json!({"transcript": "  "})), None);
        assert_eq!(transcript_text(&json!({"transcript": null})), None);
    }

    #[test]
    fn parse_list_reads_data_has_more_and_bare_array() {
        let (items, has_more) = parse_list(&json!({"object":"list","url":"/","has_more":true,"data":[1,2]}));
        assert_eq!(items.len(), 2);
        assert!(has_more);
        let (items, has_more) = parse_list(&json!({"object":"list","url":"/","has_more":false,"data":[1]}));
        assert_eq!(items.len(), 1);
        assert!(!has_more);
        let (items, has_more) = parse_list(&json!([1, 2, 3]));
        assert_eq!(items.len(), 3);
        assert!(!has_more);
    }

    #[test]
    fn full_pull_writes_contract_raw_transcript_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new();
        api.add_page(None, vec![meeting_full("m1")], false);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1));
        assert_eq!(out.counts.get("transcripts"), Some(&1));
        assert_eq!(out.counts.get("raw"), Some(&1));

        // Contract row in the ts-month partition.
        let start_rfc = ms_to_rfc3339(1749571200000);
        let key = Partition::Month.key(&to_local(&start_rfc)).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.guid, "m1");
        assert!(!m.transcript_ref.is_empty(), "transcript_ref set");
        assert!(m.transcript_ref.ends_with("m1.md"));

        // Sidecar written.
        let sidecar = v.root().join("meetings/read-ai/raw/transcripts/m1.md");
        assert!(sidecar.exists());
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("Alice:"));

        // Watermark advanced.
        let state = v.read_read_ai_sync();
        assert!(state.last_meeting_ts.is_some());
    }

    #[test]
    fn async_transcript_upserts_same_row_no_duplicate() {
        let v = temp_vault("async");

        // Poll 1: meeting listed, no transcript yet.
        let api1 = MockApi::new();
        api1.add_page(None, vec![meeting_pending("p1")], false);
        let out1 = pull_with(&v, &api1).unwrap();
        assert_eq!(out1.counts.get("transcripts"), Some(&0));

        let start_rfc = ms_to_rfc3339(1749657600000);
        let key = Partition::Month.key(&to_local(&start_rfc)).unwrap().to_string();
        let rows1: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows1.len(), 1);
        assert_eq!(rows1[0].transcript_ref, "", "no transcript_ref on first poll");
        assert!(v.read_read_ai_sync().last_meeting_ts.is_none(), "watermark not advanced yet");

        // Poll 2: same meeting now has its transcript.
        let api2 = MockApi::new();
        api2.add_page(None, vec![meeting_pending_with_transcript("p1")], false);
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("transcripts"), Some(&1));

        let rows2: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows2.len(), 1, "STILL exactly one row — upsert, not duplicate");
        assert!(!rows2[0].transcript_ref.is_empty());
        assert!(v.root().join("meetings/read-ai/raw/transcripts/p1.md").exists());
        assert!(v.read_read_ai_sync().last_meeting_ts.is_some());
    }

    #[test]
    fn cursor_pagination_drains_all_pages() {
        let v = temp_vault("paginate");
        let api = MockApi::new();
        // Cursor = last item's `id` from the previous page's data array.
        api.add_page(None, vec![meeting_full("m1")], true);
        api.add_page(Some("m1"), vec![meeting_full("m2")], true);
        api.add_page(Some("m2"), vec![meeting_full("m3")], false);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&3), "all pages' meetings landed");
        assert!(api.requested("cursor=m1"), "page 2 fetched");
        assert!(api.requested("cursor=m2"), "page 3 fetched");

        let start_rfc = ms_to_rfc3339(1749571200000);
        let key = Partition::Month.key(&to_local(&start_rfc)).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        let guids: Vec<&str> = rows.iter().map(|r| r.guid.as_str()).collect();
        assert!(guids.contains(&"m1") && guids.contains(&"m2") && guids.contains(&"m3"));
    }

    #[test]
    fn watermark_skips_old_complete_meetings_outside_recheck_window() {
        let v = temp_vault("watermark");
        v.write_read_ai_sync(&SyncState {
            last_meeting_ts: Some("2025-06-10T16:00:00+00:00".into()),
        })
        .unwrap();
        // An old meeting from 2025-01-05 is outside the 30-day recheck window.
        let mut old = meeting_full("old1");
        old.as_object_mut()
            .unwrap()
            .insert("start_time_ms".into(), json!(1736082000000i64)); // 2025-01-05T~14:40:00Z
        old.as_object_mut()
            .unwrap()
            .insert("end_time_ms".into(), json!(1736085600000i64));
        let api = MockApi::new();
        api.add_page(None, vec![old], false);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&0), "old meeting outside window skipped");
    }

    #[test]
    fn resync_same_meeting_no_duplicate_row() {
        let v = temp_vault("resync");
        let api1 = MockApi::new();
        api1.add_page(None, vec![meeting_full("m1")], false);
        pull_with(&v, &api1).unwrap();

        v.write_read_ai_sync(&SyncState::default()).unwrap();
        let api2 = MockApi::new();
        api2.add_page(None, vec![meeting_full("m1")], false);
        pull_with(&v, &api2).unwrap();

        let start_rfc = ms_to_rfc3339(1749571200000);
        let key = Partition::Month.key(&to_local(&start_rfc)).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1, "upsert by guid — one row after a re-poll");
    }

    #[test]
    fn key_never_in_cursor() {
        let v = temp_vault("secret");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "read_ai_secret_xyz".into(),
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
        assert_eq!(status.accounts[0].label, "Read.ai");

        let api = MockApi::new();
        api.add_page(None, vec![meeting_full("m9")], false);
        pull_with(&v, &api).unwrap();

        let cursor = std::fs::read_to_string(v.root().join(".trove/read-ai-sync.json")).unwrap();
        assert!(!cursor.contains("read_ai_secret_xyz"), "API key never in cursor");
        assert!(!cursor.contains("access_token"), "no token field in cursor");
    }

    #[test]
    fn connection_def_has_correct_id_and_token_paste() {
        assert_eq!(CONNECTION.id, "read-ai");
        assert!(CONNECTION.method("token-paste").is_some());
    }
}
